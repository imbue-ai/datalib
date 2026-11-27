//! A step's problems, kept in `problems.sqlite` in the step's own folder:
//! the same `problems` table every store carries (`ProblemRow`), and the
//! rules that clear it at the end of a run. A step writes the file itself
//! through [`StepFile`], or sends `problem` and `covered` lines and the
//! runner writes it through the same type (`docs/dev/step_protocol.md`
//! §"Problems: what you could not do").

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_problems::{Outcome, Problem, ProblemRow, Reason, Scope, ScopeKind, Severity, Stage};
use datalib_sql::{sql, sql_id, Sql};
use datalib_table::BulkUpsertable as _;
use datalib_time::StoredStamp;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

pub const FILE_NAME: &str = "problems.sqlite";

pub fn file_of(data_root: &Path, step_id: &str) -> PathBuf {
    data_root.join(step_id).join(FILE_NAME)
}

/// One problem as a `problem` line spells it: a [`ProblemRow`] without
/// the columns the writer fills (its id, its source, its stamps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub stage: Stage,
    pub scope_kind: ScopeKind,
    /// What the problem is about within the step: `<table>:<id>` for a
    /// record, `file:<path>` for a file, `listing:<name>` or
    /// `phase:<name>` for a part of the run. [`Covered`] matches it.
    pub scope_key: String,
    pub outcome: Outcome,
    pub reason: Reason,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub sample: String,
    #[serde(default)]
    pub item_uuid: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub upstream_entity_kind: Option<String>,
    #[serde(default)]
    pub upstream_id: Option<String>,
    #[serde(default)]
    pub upstream_account: Option<String>,
}

impl Found {
    pub fn into_row(self, source_id: &str) -> ProblemRow {
        let scope = match self.scope_kind {
            ScopeKind::Markdown => Scope::Markdown(&self.scope_key),
            ScopeKind::Entity => Scope::Entity(&self.scope_key),
        };
        let problem = Problem {
            reason: self.reason,
            field: self.field.clone(),
            path: self.path.clone(),
            rule: self.rule.clone(),
            sample: self.sample.clone(),
            severity: self.severity,
        };
        ProblemRow {
            upstream_entity_kind: self.upstream_entity_kind.clone(),
            upstream_id: self.upstream_id.clone(),
            upstream_account: self.upstream_account.clone(),
            ..ProblemRow::new(
                source_id,
                self.stage,
                scope,
                self.item_uuid.as_deref(),
                self.outcome,
                problem,
                None,
            )
        }
    }
}

/// What a run checked again, and so may clear: the body of a `covered`
/// line. A records or files kind that names neither `all_but` nor
/// `only` vouches for every one of them, which only a run that succeeds
/// can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Covered {
    All,
    /// Every `listing:` and `phase:` scope.
    Listings,
    /// The `<table>:<id>` scopes of one table: every one but `all_but`,
    /// or exactly `only`.
    Records {
        table: String,
        #[serde(default)]
        all_but: Option<Vec<String>>,
        #[serde(default)]
        only: Option<Vec<String>>,
    },
    /// The `file:<path>` scopes under a folder, but `all_but`.
    Files {
        under: String,
        #[serde(default)]
        all_but: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Succeeded,
    /// Failed, stopped, or ended `incomplete`: it is not known that every
    /// part it covered ran.
    Short,
}

/// Whether a row this run did not find again goes, given what the run
/// covered and how it ended.
pub fn clears(row: &ProblemRow, covered: &[Covered], ended: Ended) -> bool {
    let whole = ended == Ended::Succeeded;
    let but = |all_but: &Option<Vec<String>>, one: &str| match all_but {
        None => whole,
        Some(list) => !list.iter().any(|x| x == one),
    };
    let key = row.scope_key.as_str();
    covered.iter().any(|c| match c {
        Covered::All => whole,
        Covered::Listings => whole && (key.starts_with("listing:") || key.starts_with("phase:")),
        Covered::Records {
            table,
            all_but,
            only,
        } => key
            .strip_prefix(table.as_str())
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|id| match only {
                Some(only) => only.iter().any(|x| x == id),
                None => but(all_but, id),
            }),
        Covered::Files { under, all_but } => key
            .strip_prefix("file:")
            .filter(|p| is_under(p, under))
            .is_some_and(|p| but(all_but, p)),
    })
}

fn is_under(path: &str, under: &str) -> bool {
    let under = under.trim_end_matches('/');
    under.is_empty()
        || path == under
        || path
            .strip_prefix(under)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Write `rows` into the `problems` table `pool` holds, in one
/// transaction, each stamped against the row it holds under the same id
/// (`ProblemRow::stamped`), so a problem found again unchanged changes
/// nothing. Rows sharing an id are one row, their samples joined. Adds
/// every id written to `found`.
pub async fn record(
    pool: &SqlitePool,
    rows: Vec<ProblemRow>,
    now: &StoredStamp,
    found: &mut HashSet<String>,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let insert = Sql::dynamic(
        sql_id!("step_problems.insert_problem"),
        datalib_table::insert_sql::<ProblemRow>(),
    );
    let mut tx = pool.begin().await.context("begin the problems write")?;
    let mut news = Vec::new();
    for row in one_per_id(rows) {
        let held = sql!(
            "step_problems.read_problem",
            "SELECT * FROM problems WHERE problem_uuid = ?"
        )
        .query()
        .bind(&row.problem_uuid)
        .fetch_optional(&mut *tx)
        .await
        .context("read the problem held")?
        .map(|r| ProblemRow::from_row(&r))
        .transpose()?;
        let row = row.stamped(held.as_ref(), &now.utc, now.tz_offset.as_deref());
        sql!(
            "step_problems.delete_replaced_problem",
            "DELETE FROM problems WHERE problem_uuid = ?"
        )
        .query()
        .bind(&row.problem_uuid)
        .execute(&mut *tx)
        .await
        .context("replace the problem held")?;
        // Audited: `insert` is built from `ProblemRow`'s associated
        // consts, never from row data; every value is bound.
        row.bind_into(insert.query())
            .execute(&mut *tx)
            .await
            .with_context(|| format!("record problem {}", row.problem_uuid))?;
        if row.is_news(held.as_ref()) {
            news.push(row.clone());
        }
        found.insert(row.problem_uuid);
    }
    tx.commit().await.context("commit the problems write")?;
    datalib_problems::note_recorded(&news);
    Ok(())
}

/// Remove from `pool`'s `problems` what the run covered and did not
/// find again (`found`). Returns how many rows went.
pub async fn clear(
    pool: &SqlitePool,
    found: &HashSet<String>,
    covered: &[Covered],
    ended: Ended,
) -> Result<usize> {
    let mut tx = pool.begin().await.context("begin the problems clear")?;
    let held: Vec<ProblemRow> = sql!(
        "step_problems.list_problems_to_clear",
        "SELECT * FROM problems"
    )
    .query()
    .fetch_all(&mut *tx)
    .await
    .context("read the problems held")?
    .iter()
    .map(ProblemRow::from_row)
    .collect::<Result<_>>()?;
    let mut gone = 0;
    for row in held
        .iter()
        .filter(|r| !found.contains(&r.problem_uuid) && clears(r, covered, ended))
    {
        sql!(
            "step_problems.delete_cleared_problem",
            "DELETE FROM problems WHERE problem_uuid = ?"
        )
        .query()
        .bind(&row.problem_uuid)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("clear problem {}", row.problem_uuid))?;
        gone += 1;
    }
    tx.commit().await.context("commit the problems clear")?;
    Ok(gone)
}

/// Make `pool`'s `problems` hold `rows` and nothing else, for a step
/// that finds every one of its problems again on each run
/// (`render_time_series`, `render_occurrences`). Uncommitted to a
/// doltlite store: the caller seals.
pub async fn replace(pool: &SqlitePool, rows: Vec<ProblemRow>, now: &StoredStamp) -> Result<()> {
    let mut found = HashSet::new();
    record(pool, rows, now, &mut found).await?;
    clear(pool, &found, &[Covered::All], Ended::Succeeded).await?;
    Ok(())
}

/// One row per id, in first-seen order, a later row's sample appended
/// to the first's: two reasons for one record are one problem.
fn one_per_id(rows: Vec<ProblemRow>) -> Vec<ProblemRow> {
    let mut out: Vec<ProblemRow> = Vec::with_capacity(rows.len());
    let mut at: std::collections::HashMap<String, usize> = Default::default();
    for row in rows {
        match at.get(&row.problem_uuid) {
            Some(&i) => {
                let first = &mut out[i];
                if !first.sample.split("; ").any(|s| s == row.sample) {
                    first.sample = format!("{}; {}", first.sample, row.sample);
                }
            }
            None => {
                at.insert(row.problem_uuid.clone(), out.len());
                out.push(row);
            }
        }
    }
    out
}

/// One run's writer of one step's own `problems.sqlite`:
/// [`record`](Self::record) as problems are found, then
/// [`finish`](Self::finish) once. The file's one writer while it is open.
pub struct StepFile {
    pool: SqlitePool,
    now: StoredStamp,
    found: HashSet<String>,
}

impl StepFile {
    pub async fn open(data_root: &Path, step_id: &str, now: StoredStamp) -> Result<Self> {
        let path = file_of(data_root, step_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .create_if_missing(true)
            // Readers read it while the step is still writing.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        if let Err(e) = set_up(&pool).await {
            pool.close().await;
            return Err(e)
                .with_context(|| format!("set up the problems table in {}", path.display()));
        }
        Ok(StepFile {
            pool,
            now,
            found: HashSet::new(),
        })
    }

    pub async fn record(&mut self, rows: Vec<ProblemRow>) -> Result<()> {
        record(&self.pool, rows, &self.now, &mut self.found).await
    }

    /// The shape a step's own `problems.sqlite` is in.
    pub fn shape() -> datalib_store_meta::Shape {
        datalib_store_meta::Shape {
            tables: datalib_store_meta::schema_hash(datalib_problems::DDL.iter().map(|(_, d)| *d)),
            indexes: None,
            versions: datalib_store_meta::Versions {
                schema: 0,
                shared: datalib_problems::SHAPE_VERSION,
            },
        }
    }

    /// End the run: clear what it covered and did not find again, and
    /// close the file. Returns how many rows went.
    pub async fn finish(self, covered: &[Covered], ended: Ended) -> Result<usize> {
        let result = clear(&self.pool, &self.found, covered, ended).await;
        self.pool.close().await;
        result
    }
}

/// The table in this build's shape, its rows dropped when the file holds
/// another: they are what an earlier run found, and the run opening it
/// finds them again.
async fn set_up(pool: &SqlitePool) -> Result<()> {
    let want = StepFile::shape();
    let have = datalib_store_meta::read(pool).await?.map(|m| m.shape());
    if have.as_ref() != Some(&want) {
        sql!(
            "step_problems.drop_problems_table",
            "DROP TABLE IF EXISTS problems"
        )
        .query()
        .execute(pool)
        .await?;
    }
    for (_, ddl) in datalib_problems::DDL {
        Sql::dynamic(sql_id!("step_problems.create_schema"), *ddl)
            .query()
            .execute(pool)
            .await?;
    }
    datalib_store_meta::write(pool, datalib_store_meta::StoreKind::StepProblems, &want).await?;
    Ok(())
}

/// Every row of the file at `path`, read-only; none when there is no file.
pub async fn read(path: &Path) -> Result<Vec<ProblemRow>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(
        read_with(path, datalib_sqlite_open::file(path).read_only(true))
            .await?
            .rows,
    )
}

/// What one store holds that a reader of problems wants: its `problems`,
/// and, in a render store, its `entity_items` (render's map from a raw
/// record a download's problem names to the grid row it belongs to). Its
/// `_datalib_meta` says which it holds; a file that records no kind
/// holding problems gives none.
#[derive(Debug, Clone, Default)]
struct Held {
    rows: Vec<ProblemRow>,
    items: Vec<(String, String)>,
    /// What it holds of these is in another shape than this build reads:
    /// its step writes it in this shape when it runs again.
    other_shape: bool,
}

async fn read_with(path: &Path, opts: SqliteConnectOptions) -> Result<Held> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let held = async {
        let mut held = Held::default();
        let Some(meta) = datalib_store_meta::read(&pool).await? else {
            // A step's own file always records its shape, so one that
            // records none is an older build's; any other such file holds
            // no problems.
            held.other_shape = path.file_name().is_some_and(|n| n == FILE_NAME);
            return anyhow::Ok(held);
        };
        let Some(kind) = meta.store_kind.filter(|k| k.holds_problems()) else {
            return anyhow::Ok(held);
        };
        if meta.shared_schema_version != datalib_problems::SHAPE_VERSION {
            held.other_shape = true;
            return anyhow::Ok(held);
        }
        // The render store's own tables have no version of their own to read
        // here, and this crate sits below the one that knows their shape.
        if kind == datalib_store_meta::StoreKind::Render && !has_entity_items(&pool).await? {
            held.other_shape = true;
            return anyhow::Ok(held);
        }
        held.rows = sql!(
            "step_problems.list_problems",
            "SELECT * FROM problems ORDER BY scope_key, problem_uuid"
        )
        .query()
        .fetch_all(&pool)
        .await?
        .iter()
        .map(ProblemRow::from_row)
        .collect::<Result<_>>()?;
        if kind == datalib_store_meta::StoreKind::Render {
            held.items = sql!(
                "step_problems.list_entity_items",
                "SELECT entity_key, item_uuid FROM entity_items ORDER BY entity_key"
            )
            .query_as()
            .fetch_all(&pool)
            .await?;
        }
        anyhow::Ok(held)
    }
    .await
    .with_context(|| format!("read the problems in {}", path.display()));
    pool.close().await;
    held
}

async fn has_entity_items(pool: &SqlitePool) -> Result<bool> {
    let n: i64 = sql!(
        "step_problems.has_entity_items_table",
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'entity_items'"
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// What a file looked like when it was read: its modification time and
/// size, and its write-ahead log's, which a plain SQLite writer grows long
/// before it touches the file itself.
type Seen = Vec<(Option<std::time::SystemTime>, Option<u64>)>;

fn seen(file: &Path) -> Seen {
    let mut wal = file.as_os_str().to_owned();
    wal.push("-wal");
    [file.to_path_buf(), PathBuf::from(wal)]
        .iter()
        .map(|p| match std::fs::metadata(p) {
            Ok(m) => (m.modified().ok(), Some(m.len())),
            Err(_) => (None, None),
        })
        .collect()
}

/// What each file held when last read, so a reader asked again (the
/// Manage screen asks on every refresh) opens only the files that moved.
static READ: std::sync::Mutex<std::collections::BTreeMap<PathBuf, (Seen, Held)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

async fn read_cached(file: &Path, plain: bool) -> Result<Held> {
    let before = seen(file);
    if let Some((was, held)) = READ.lock().expect("unpoisoned").get(file) {
        if *was == before {
            return Ok(held.clone());
        }
    }
    let opts = if plain {
        datalib_sqlite_open::file(file).read_only(true)
    } else {
        // A doltlite store as its `main` commit has it: what its writer
        // last sealed, read without waiting on the writer.
        datalib_sqlite_open::revision(file, "main").read_only(true)
    };
    let held = read_with(file, opts).await?;
    READ.lock()
        .expect("unpoisoned")
        .insert(file.to_path_buf(), (before, held.clone()));
    Ok(held)
}

/// One step's problems, or why they could not be read.
#[derive(Debug)]
pub struct OfStep {
    pub step_id: String,
    pub rows: Vec<ProblemRow>,
    /// `(entity_key, item_uuid)`: render's map from a raw record a
    /// download's problem names to the grid row it belongs to.
    pub items: Vec<(String, String)>,
    pub unread: Vec<String>,
    /// Stores in another shape than this build reads, whose problems are
    /// read once the step runs again and rewrites them. Not a failure: a
    /// step turned off after an upgrade keeps one for good.
    pub other_shape: Vec<PathBuf>,
}

/// What a step's problems are: the `problems` table of every store
/// directly in its folder (a `.doltlite_db` read at `main`, or a plain
/// `.sqlite` file, its own `problems.sqlite` among them).
pub async fn of_step(data_root: &Path, step_id: &str) -> OfStep {
    let mut out = OfStep {
        step_id: step_id.to_string(),
        rows: Vec::new(),
        items: Vec::new(),
        unread: Vec::new(),
        other_shape: Vec::new(),
    };
    let dir = data_root.join(step_id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    for file in files {
        let read = match file.extension().and_then(|e| e.to_str()) {
            Some(ext @ ("doltlite_db" | "sqlite")) => read_cached(&file, ext == "sqlite").await,
            _ => continue,
        };
        match read {
            Ok(held) => {
                if held.other_shape {
                    out.other_shape.push(file.clone());
                }
                out.rows.extend(held.rows);
                out.items.extend(held.items);
            }
            Err(e) => out.unread.push(format!("{e:#}")),
        }
    }
    out
}

/// Every step's problems under `data_root`: [`of_step`] for each
/// `<group>/<function>` folder, in order, each download's problem linked
/// to the grid row its record belongs to ([`link`]). A folder with no
/// store holding either table gives none.
pub async fn of_steps(data_root: &Path) -> Vec<OfStep> {
    let mut steps = Vec::new();
    for group in sorted_dirs(data_root) {
        for function in sorted_dirs(&group) {
            let (Some(g), Some(f)) = (
                group.file_name().and_then(|n| n.to_str()),
                function.file_name().and_then(|n| n.to_str()),
            ) else {
                continue;
            };
            steps.push(format!("{g}/{f}"));
        }
    }
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let read = of_step(data_root, &step).await;
        if !read.rows.is_empty()
            || !read.items.is_empty()
            || !read.unread.is_empty()
            || !read.other_shape.is_empty()
        {
            out.push(read);
        }
    }
    link(&mut out);
    out
}

/// One thing [`say_unread`] says about a step: why a store could not be
/// read, or which store is in another shape.
type Said = (String, String, bool);

/// What a reader of every step's problems logs of what it could not read:
/// each thing once, the first pass it appears in, and again only after a
/// pass without it. The Manage screen reads them on every refresh, so
/// saying each every time would fill the log (26,830 lines in two hours
/// on one library). One reader per process, so one record of what it said.
pub fn say_unread(steps: &[OfStep]) {
    static SAID: std::sync::Mutex<std::collections::BTreeSet<Said>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let now = unread_of(steps);
    let mut said = SAID.lock().expect("unpoisoned");
    for (step, what, older) in newly(&said, &now) {
        if older {
            tracing::info!(
                step = %step,
                store = %what,
                "this step's problems are in another shape than this build reads; they are \
                 read once the step runs again"
            );
        } else {
            tracing::error!(step = %step, error = %what, "could not read a step's problems");
        }
    }
    *said = now;
}

fn unread_of(steps: &[OfStep]) -> std::collections::BTreeSet<Said> {
    steps
        .iter()
        .flat_map(|s| {
            let why = s
                .unread
                .iter()
                .map(|w| (s.step_id.clone(), w.clone(), false));
            let older = s
                .other_shape
                .iter()
                .map(|p| (s.step_id.clone(), p.display().to_string(), true));
            why.chain(older)
        })
        .collect()
}

/// What is in `now` and was not in `said`: what to say this pass.
fn newly(
    said: &std::collections::BTreeSet<Said>,
    now: &std::collections::BTreeSet<Said>,
) -> Vec<Said> {
    now.difference(said).cloned().collect()
}

/// Fill in `item_uuid` on each download problem that names none, from
/// the map a step of the same source keeps (`entity_items`, by the raw
/// record the problem names). A record with no row stays unlinked.
pub fn link(steps: &mut [OfStep]) {
    let group = |step_id: &str| {
        step_id
            .rsplit_once('/')
            .map_or(step_id, |(g, _)| g)
            .to_string()
    };
    let items: std::collections::HashMap<(String, String), String> = steps
        .iter()
        .flat_map(|s| {
            let g = group(&s.step_id);
            s.items
                .iter()
                .map(move |(entity, item)| ((g.clone(), entity.clone()), item.clone()))
        })
        .collect();
    for row in steps.iter_mut().flat_map(|s| s.rows.iter_mut()) {
        if row.item_uuid.is_some()
            || row.stage != Stage::Fetch
            || row.scope_kind != ScopeKind::Entity
        {
            continue;
        }
        row.item_uuid = items
            .get(&(row.source_id.clone(), row.scope_key.clone()))
            .cloned();
    }
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests;
