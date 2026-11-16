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
use datalib_table::BulkUpsertable;
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

fn insert_sql() -> String {
    let cols: Vec<&str> = std::iter::once(ProblemRow::ID_COLUMN)
        .chain(ProblemRow::TYPED_COLUMNS.iter().copied())
        .collect();
    format!(
        "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
        ProblemRow::TABLE,
        cols.join(", "),
        vec!["?"; cols.len()].join(", ")
    )
}

/// One run's writer of one step's file: [`record`](Self::record) as
/// problems are found, then [`finish`](Self::finish) once. The file's one
/// writer while it is open.
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
        for (_, ddl) in datalib_problems::DDL {
            if let Err(e) = sqlx::query(*ddl).execute(&pool).await {
                pool.close().await;
                return Err(e)
                    .with_context(|| format!("create the problems table in {}", path.display()));
            }
        }
        Ok(StepFile {
            pool,
            now,
            found: HashSet::new(),
        })
    }

    /// Write what this run found, in one transaction, each row stamped
    /// against the one the file holds under the same id.
    pub async fn record(&mut self, rows: Vec<ProblemRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let insert = insert_sql();
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin the problems write")?;
        let mut news = Vec::new();
        for row in rows {
            let held = sqlx::query("SELECT * FROM problems WHERE problem_uuid = ?")
                .bind(&row.problem_uuid)
                .fetch_optional(&mut *tx)
                .await
                .context("read the problem held")?
                .map(|r| ProblemRow::from_row(&r))
                .transpose()?;
            let row = row.stamped(held.as_ref(), &self.now.utc, self.now.tz_offset.as_deref());
            // Audited: `insert` is built from `ProblemRow`'s associated
            // consts, never from row data; every value is bound.
            row.bind_into(sqlx::query(sqlx::AssertSqlSafe(insert.clone())))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("record problem {}", row.problem_uuid))?;
            if row.is_news(held.as_ref()) {
                news.push(row.clone());
            }
            self.found.insert(row.problem_uuid);
        }
        tx.commit().await.context("commit the problems write")?;
        datalib_problems::note_recorded(&news);
        Ok(())
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
        let held: Vec<ProblemRow> = sqlx::query("SELECT * FROM problems")
            .fetch_all(&mut *tx)
            .await
            .context("read the problems held")?
            .iter()
            .map(ProblemRow::from_row)
            .collect::<Result<_>>()?;
        let mut gone = 0;
        for row in held
            .iter()
            .filter(|r| !self.found.contains(&r.problem_uuid) && clears(r, covered, ended))
        {
            sqlx::query("DELETE FROM problems WHERE problem_uuid = ?")
                .bind(&row.problem_uuid)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("clear problem {}", row.problem_uuid))?;
            gone += 1;
        }
        tx.commit().await.context("commit the problems clear")?;
        Ok(gone)
    }
}

/// Every row of the file at `path`, read-only; none when there is no file.
pub async fn read(path: &Path) -> Result<Vec<ProblemRow>> {
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
    let rows = sqlx::query("SELECT * FROM problems ORDER BY scope_key, problem_uuid")
        .fetch_all(&pool)
        .await
        .with_context(|| format!("read {}", path.display()));
    pool.close().await;
    rows?.iter().map(ProblemRow::from_row).collect()
}

#[cfg(test)]
mod tests;
