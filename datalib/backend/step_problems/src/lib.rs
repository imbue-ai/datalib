//! A step's problems, kept in `problems.sqlite` in the step's own folder:
//! the one shape every writer uses, and the rules that stamp and clear its
//! rows. A step writes the file itself through [`StepFile`], or sends
//! `problem` and `covered` events and the runner writes it through the
//! same type (`docs/dev/step_protocol.md` §"Problems").

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_problems::Severity;
use datalib_time::StoredStamp;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row as _;

pub const FILE_NAME: &str = "problems.sqlite";

/// Where the step `step_id` keeps its problems under `data_root`.
pub fn file_of(data_root: &Path, step_id: &str) -> PathBuf {
    data_root.join(step_id).join(FILE_NAME)
}

/// What a problem is about, in the terms its step held when it arose.
/// Every field may be empty; all empty is the step's run as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct About {
    /// The upstream record: the values a grid row carries as
    /// `upstream_entity_kind`, `upstream_id` and `upstream_account`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// A node's uuid, for a step that held the node itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// A file, relative to the folder the step reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// One problem as a step reports it: the body of a `problem` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    /// The step's own name for the problem, the same on every run that
    /// finds it. By convention `listing:<name>`, `phase:<name>` or
    /// `record:<table>:<id>`, which is what [`Covered`] matches.
    pub key: String,
    pub severity: Severity,
    pub outcome: String,
    pub reason: String,
    #[serde(default)]
    pub sample: String,
    #[serde(default)]
    pub about: About,
}

/// What a run re-checked, and so may clear: the body of a `covered`
/// event. A record or file kind with no `all_but` vouches for every one
/// of them, which only a run that succeeds can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Covered {
    /// Everything the step can find.
    All,
    /// Every `listing:` and `phase:` key.
    Listings,
    /// Every `record:<table>:` key, but the ids named.
    Records {
        table: String,
        #[serde(default)]
        all_but: Option<Vec<String>>,
    },
    /// Every problem about a file under `under`, but the paths named.
    Files {
        under: String,
        #[serde(default)]
        all_but: Option<Vec<String>>,
    },
}

/// How the run ended, as far as clearing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Succeeded,
    /// Failed, stopped, or ended `incomplete`: it is not known that every
    /// part it covered ran.
    Short,
}

/// One row of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub key: String,
    pub problem_uuid: String,
    pub severity: Severity,
    pub outcome: String,
    pub reason: String,
    pub sample: String,
    pub about: About,
    pub first_seen_at_utc: String,
    pub changed_at_utc: String,
    pub tz_offset: Option<String>,
    /// The run that last found it.
    pub run: String,
}

/// `found` as the file keeps it, given the row it holds under the same
/// key: found again unchanged, it keeps every stamp; changed, it keeps
/// when it was first seen and is stamped `now`.
pub fn stamped(
    step_id: &str,
    found: Found,
    held: Option<&Row>,
    now: &StoredStamp,
    run: &str,
) -> Row {
    let same = held.is_some_and(|h| {
        h.severity == found.severity
            && h.outcome == found.outcome
            && h.reason == found.reason
            && h.sample == found.sample
            && h.about == found.about
    });
    let (first_seen_at_utc, changed_at_utc, tz_offset) = match held {
        Some(h) if same => (
            h.first_seen_at_utc.clone(),
            h.changed_at_utc.clone(),
            h.tz_offset.clone(),
        ),
        Some(h) => (
            h.first_seen_at_utc.clone(),
            now.utc.clone(),
            now.tz_offset.clone(),
        ),
        None => (now.utc.clone(), now.utc.clone(), now.tz_offset.clone()),
    };
    Row {
        problem_uuid: datalib_id::step_problem_id(step_id, &found.key),
        key: found.key,
        severity: found.severity,
        outcome: found.outcome,
        reason: found.reason,
        sample: found.sample,
        about: found.about,
        first_seen_at_utc,
        changed_at_utc,
        tz_offset,
        run: run.to_string(),
    }
}

/// Whether a row this run did not find again goes, given what the run
/// covered and how it ended.
pub fn clears(row: &Row, covered: &[Covered], ended: Ended) -> bool {
    let whole = ended == Ended::Succeeded;
    let but = |all_but: &Option<Vec<String>>, one: &str| match all_but {
        None => whole,
        Some(list) => !list.iter().any(|x| x == one),
    };
    covered.iter().any(|c| match c {
        Covered::All => whole,
        Covered::Listings => {
            whole && (row.key.starts_with("listing:") || row.key.starts_with("phase:"))
        }
        Covered::Records { table, all_but } => row
            .key
            .strip_prefix("record:")
            .and_then(|rest| rest.strip_prefix(table.as_str()))
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|id| but(all_but, id)),
        Covered::Files { under, all_but } => row
            .about
            .path
            .as_deref()
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

const DDL: &str = "CREATE TABLE IF NOT EXISTS problems (
    key TEXT PRIMARY KEY NOT NULL,
    problem_uuid TEXT NOT NULL,
    severity TEXT NOT NULL,
    outcome TEXT NOT NULL,
    reason TEXT NOT NULL,
    sample TEXT NOT NULL,
    about_kind TEXT,
    about_id TEXT,
    about_account TEXT,
    about_node TEXT,
    about_path TEXT,
    first_seen_at_utc TEXT NOT NULL,
    changed_at_utc TEXT NOT NULL,
    tz_offset TEXT,
    run TEXT NOT NULL
)";

const INSERT: &str = "INSERT OR REPLACE INTO problems (key, problem_uuid, severity, outcome, \
     reason, sample, about_kind, about_id, about_account, about_node, about_path, \
     first_seen_at_utc, changed_at_utc, tz_offset, run) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

fn row_of(r: &SqliteRow) -> Result<Row> {
    let severity: String = r.try_get("severity")?;
    Ok(Row {
        key: r.try_get("key")?,
        problem_uuid: r.try_get("problem_uuid")?,
        severity: Severity::parse(&severity)
            .with_context(|| format!("a severity this build does not know: {severity:?}"))?,
        outcome: r.try_get("outcome")?,
        reason: r.try_get("reason")?,
        sample: r.try_get("sample")?,
        about: About {
            kind: r.try_get("about_kind")?,
            id: r.try_get("about_id")?,
            account: r.try_get("about_account")?,
            node: r.try_get("about_node")?,
            path: r.try_get("about_path")?,
        },
        first_seen_at_utc: r.try_get("first_seen_at_utc")?,
        changed_at_utc: r.try_get("changed_at_utc")?,
        tz_offset: r.try_get("tz_offset")?,
        run: r.try_get("run")?,
    })
}

/// One run's writer of one step's file: [`record`](Self::record) as
/// problems are found, then [`finish`](Self::finish) once. The file's one
/// writer while it is open.
pub struct StepFile {
    pool: SqlitePool,
    step_id: String,
    run: String,
    now: StoredStamp,
}

impl StepFile {
    /// Open (creating it) the file of `step_id` for the run `run`, whose
    /// stamps are `now`.
    pub async fn open(
        data_root: &Path,
        step_id: &str,
        run: &str,
        now: StoredStamp,
    ) -> Result<Self> {
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
        sqlx::query(DDL)
            .execute(&pool)
            .await
            .with_context(|| format!("create the problems table in {}", path.display()))?;
        Ok(StepFile {
            pool,
            step_id: step_id.to_string(),
            run: run.to_string(),
            now,
        })
    }

    /// Write what this run found, in one transaction. A key found twice
    /// in one run is one row, the later one's.
    pub async fn record(&self, found: Vec<Found>) -> Result<()> {
        if found.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin the problems write")?;
        for f in found {
            let held = sqlx::query("SELECT * FROM problems WHERE key = ?")
                .bind(&f.key)
                .fetch_optional(&mut *tx)
                .await
                .context("read the problem held")?
                .map(|r| row_of(&r))
                .transpose()?;
            let row = stamped(&self.step_id, f, held.as_ref(), &self.now, &self.run);
            sqlx::query(INSERT)
                .bind(&row.key)
                .bind(&row.problem_uuid)
                .bind(row.severity.as_str())
                .bind(&row.outcome)
                .bind(&row.reason)
                .bind(&row.sample)
                .bind(&row.about.kind)
                .bind(&row.about.id)
                .bind(&row.about.account)
                .bind(&row.about.node)
                .bind(&row.about.path)
                .bind(&row.first_seen_at_utc)
                .bind(&row.changed_at_utc)
                .bind(&row.tz_offset)
                .bind(&row.run)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("record problem {}", row.key))?;
        }
        tx.commit().await.context("commit the problems write")
    }

    /// End the run: remove what it covered and did not find again, and
    /// close the file. Returns how many rows went.
    pub async fn finish(self, covered: &[Covered], ended: Ended) -> Result<usize> {
        let result = self.clear(covered, ended).await;
        self.pool.close().await;
        result
    }

    async fn clear(&self, covered: &[Covered], ended: Ended) -> Result<usize> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin the problems clear")?;
        let earlier: Vec<Row> = sqlx::query("SELECT * FROM problems WHERE run != ?")
            .bind(&self.run)
            .fetch_all(&mut *tx)
            .await
            .context("read the problems held")?
            .iter()
            .map(row_of)
            .collect::<Result<_>>()?;
        let mut gone = 0;
        for row in earlier.iter().filter(|r| clears(r, covered, ended)) {
            sqlx::query("DELETE FROM problems WHERE key = ?")
                .bind(&row.key)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("clear problem {}", row.key))?;
            gone += 1;
        }
        tx.commit().await.context("commit the problems clear")?;
        Ok(gone)
    }
}

/// Every row of the file at `path`, read-only; none when there is no file.
pub async fn read(path: &Path) -> Result<Vec<Row>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(path).read_only(true))
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let rows = sqlx::query("SELECT * FROM problems ORDER BY key")
        .fetch_all(&pool)
        .await
        .with_context(|| format!("read {}", path.display()));
    pool.close().await;
    rows?.iter().map(row_of).collect()
}

#[cfg(test)]
mod tests;
