//! `system/supervisor.sqlite`: the tables anyone writes, and the tables
//! only the loop writes. Anyone writes intent into it — open a request,
//! ask for one to stop, turn a step off or on, open a job — and the one
//! process running the loop reads it and writes back how each request
//! ended, and what each step did (`record.rs`). Plain SQLite in rollback-journal mode, several writing
//! processes at once. Its shape moves by the rungs of [`LADDER`]: a build
//! older than the store's refuses it. Every commit is announced (`announce.rs`), which is how the loop
//! and the server hear of it. `dag/README.md` §"The record".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use strum::{EnumString, IntoStaticStr, VariantArray};

use super::pass::{PassRow, PassState};
use crate::step::StoreBytes;
use datalib_sql::{sql, sql_id, Sql};

/// How a store's tables reach this build's shape, after what
/// `CREATE TABLE IF NOT EXISTS` makes on its own. Every store from before
/// the ladder records version 5, with whichever of the `steps` table's
/// later columns its release had.
const LADDER: &[datalib_store_meta::Migration] = &[
    datalib_store_meta::Migration {
        version: 6,
        name: "steps in one shape; launch_passes goes",
        apply: |conn| {
            Box::pin(async move {
                sql!(
                    "dag.supervisor.migrate_v6_rename_old_steps",
                    "ALTER TABLE steps RENAME TO steps_v5"
                )
                .query()
                .execute(&mut *conn)
                .await?;
                Sql::ddl(
                    sql_id!("dag.supervisor.migrate_v6_create_steps"),
                    super::record::STEPS,
                )
                .query()
                .execute(&mut *conn)
                .await?;
                STEPS_FROM_V5.raw().execute(&mut *conn).await?;
                Ok(())
            })
        },
    },
    datalib_store_meta::Migration {
        version: 7,
        name: "wipes become jobs, which the prepare pass is one kind of",
        apply: |conn| {
            Box::pin(async move {
                let has = |table: &'static str| {
                    sql!(
                        "dag.supervisor.migrate_v7_has_table",
                        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?"
                    )
                    .query_scalar::<i64>()
                    .bind(table)
                };
                if has("wipes").fetch_one(&mut *conn).await? > 0 {
                    sql!(
                        "dag.supervisor.migrate_v7_rename_wipes",
                        "DROP INDEX IF EXISTS wipes_open; ALTER TABLE wipes RENAME TO jobs"
                    )
                    .raw()
                    .execute(&mut *conn)
                    .await?;
                }
                if has("wipe_steps").fetch_one(&mut *conn).await? > 0 {
                    sql!(
                        "dag.supervisor.migrate_v7_rename_wipe_steps",
                        "ALTER TABLE wipe_steps RENAME TO job_steps; \
                         ALTER TABLE job_steps RENAME COLUMN wipe_id TO job_id"
                    )
                    .raw()
                    .execute(&mut *conn)
                    .await?;
                }
                Ok(())
            })
        },
    },
];

/// Where this build's tables stand. A store at a higher version was
/// written by a newer build, whose columns this one would not fill.
const SCHEMA_VERSION: u32 = LADDER[LADDER.len() - 1].version;

/// Each step's `steps` row, from the columns every release since 0.38
/// had. What it leaves behind a release may not have had, and the loop
/// writes again: `needs_rerun` at the prepare pass,
/// `waiting_for` on its next tick, `last_failure_kind` at the step's next
/// run. `launch_passes` goes with it: nothing reads it.
const STEPS_FROM_V5: Sql = sql!(
    "dag.supervisor.migrate_v6_copy_steps_drop_old",
    "INSERT INTO steps (step, succeeded, fingerprint, reads, \
     last_run_id, last_started_at_utc, last_finished_at_utc, last_status, last_attempts, \
     last_error, last_success_at_utc, tz_offset, state, state_detail, turned_off_by, requests) \
     SELECT step, succeeded, fingerprint, reads, last_run_id, last_started_at_utc, \
     last_finished_at_utc, last_status, last_attempts, last_error, last_success_at_utc, \
     tz_offset, state, state_detail, turned_off_by, requests FROM steps_v5; \
     DROP TABLE steps_v5; \
     DROP TABLE IF EXISTS launch_passes"
);

const DDL: [&str; 4] = [
    "CREATE TABLE IF NOT EXISTS requests (
        id TEXT PRIMARY KEY,
        roots TEXT NOT NULL,
        opened_by TEXT NOT NULL,
        opened_at_utc TEXT NOT NULL,
        tz_offset TEXT NOT NULL,
        stop_requested_by TEXT,
        stop_requested_at_utc TEXT,
        closed_at_utc TEXT,
        outcome TEXT,
        failed_step TEXT
    )",
    "CREATE TABLE IF NOT EXISTS turned_off (
        step TEXT PRIMARY KEY,
        turned_off_by TEXT NOT NULL,
        turned_off_at_utc TEXT NOT NULL,
        tz_offset TEXT NOT NULL
    )",
    // Work the loop does that is not a step's run: a reset, a purge, a
    // compaction or collection, the prepare pass. `targets` is a JSON
    // list: step ids for a reset, group ids for a purge. `error` is set
    // when it closed without being done.
    "CREATE TABLE IF NOT EXISTS jobs (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        targets TEXT NOT NULL,
        opened_by TEXT NOT NULL,
        opened_at_utc TEXT NOT NULL,
        tz_offset TEXT NOT NULL,
        closed_at_utc TEXT,
        error TEXT
    )",
    // How a compaction, a collection or the prepare pass stands with each
    // step it runs its verb for, in the order it runs them: what the app's
    // pill and its upgrade screen count. `state` is a `PassState` word.
    "CREATE TABLE IF NOT EXISTS job_steps (
        job_id TEXT NOT NULL,
        ord INTEGER NOT NULL,
        step TEXT NOT NULL,
        state TEXT NOT NULL,
        error TEXT,
        before_bytes INTEGER,
        after_bytes INTEGER,
        PRIMARY KEY (job_id, ord)
    )",
];

/// `requests` and `jobs` keep every row they were ever given; the loop
/// reads only the open ones, and these find them without the rest.
const INDEXES: [&str; 2] = [
    "CREATE INDEX IF NOT EXISTS requests_open ON requests (closed_at_utc, opened_at_utc, id)",
    "CREATE INDEX IF NOT EXISTS jobs_open ON jobs (closed_at_utc, opened_at_utc, id)",
];

const OPEN_REQUESTS: Sql = sql!(
    "dag.supervisor.find_open_requests",
    "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
     FROM requests INDEXED BY requests_open \
     WHERE closed_at_utc IS NULL ORDER BY opened_at_utc, id"
);

const OPEN_JOBS: Sql = sql!(
    "dag.supervisor.find_open_jobs",
    "SELECT id, kind, targets, opened_by, closed_at_utc, error \
     FROM jobs INDEXED BY jobs_open \
     WHERE closed_at_utc IS NULL ORDER BY opened_at_utc, id"
);

/// Long enough to outlast another process's write transaction, which is
/// a single row here.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How a request ended.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestOutcome {
    Done,
    Failed,
    Stopped,
}

impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// What a job does: a reset empties what steps wrote, a purge deletes
/// the trees of groups the config no longer names, a compaction folds
/// away history nothing reads and collects the rest, a collection deletes
/// only what no commit reaches (`step_protocol.md` § Compact and collect).
/// A prepare is the prepare pass, which the loop opens for itself each
/// time it starts and records its progress in (`dag/README.md` §"Upgrading a root").
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum JobKind {
    Reset,
    Purge,
    Compact,
    Collect,
    Prepare,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The verb a compaction or a collection runs its steps with to give
    /// back disk.
    pub fn shrink(self) -> Option<crate::subprocess::Shrink> {
        match self {
            JobKind::Compact => Some(crate::subprocess::Shrink::Compact),
            JobKind::Collect => Some(crate::subprocess::Shrink::Collect),
            JobKind::Reset | JobKind::Purge | JobKind::Prepare => None,
        }
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One row of `jobs`. The loop carries a wipe out as soon as the steps
/// it touches have stopped, and closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: String,
    /// `None` for a kind a newer build wrote.
    pub kind: Option<JobKind>,
    pub targets: Vec<String>,
    pub opened_by: String,
    /// `None` while it is open; then `Some(None)` if it was done, or the
    /// reason it was not.
    pub closed: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub id: String,
    /// The step ids it was opened on; its scope is these and everything
    /// downstream of them.
    pub roots: Vec<String>,
    pub opened_by: String,
    pub stop_requested_by: Option<String>,
    /// `None` while it is open. An outcome a newer build wrote that this
    /// one cannot name reads as closed all the same.
    pub closed: Option<Option<RequestOutcome>>,
    pub failed_step: Option<String>,
}

pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
    listeners: PathBuf,
    /// Who this store is in its announcements: tests in one binary share
    /// a pid, so the pid alone does not say.
    me: String,
}

static NEXT_STORE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Store {
    pub async fn open(data_root: &Path) -> Result<Store> {
        let path = datalib_runtime::layout::supervisor_db(data_root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        // One connection, never replaced: `PRAGMA data_version` is per
        // connection, and a replacement would read as a change.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options(&path))
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let store = Store {
            pool,
            path: path.clone(),
            listeners: super::announce::listeners_dir(data_root),
            me: format!(
                "store-{}-{}",
                std::process::id(),
                NEXT_STORE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        };
        store.run_ladder(&path).await?;
        let ddl = || {
            DDL.into_iter()
                .chain(super::record::DDL)
                .chain(INDEXES)
                .chain(super::record::INDEXES)
        };
        for stmt in ddl() {
            Sql::dynamic(sql_id!("dag.supervisor.create_schema"), stmt)
                .query()
                .execute(&store.pool)
                .await?;
        }
        datalib_store_meta::write(
            &store.pool,
            datalib_store_meta::StoreKind::Supervisor,
            &datalib_store_meta::Shape {
                tables: datalib_store_meta::schema_hash(ddl()),
                indexes: None,
                versions: datalib_store_meta::Versions {
                    schema: SCHEMA_VERSION,
                    shared: 0,
                },
            },
        )
        .await?;
        store.announce("store opened");
        Ok(store)
    }

    /// Run the rungs above the store's version, before the DDL, so
    /// `CREATE TABLE IF NOT EXISTS` finds each table in its new shape. One
    /// transaction, taken for writing first, so two processes opening at
    /// once run them once. A new store has no rung to run: the DDL makes
    /// it in this build's shape.
    async fn run_ladder(&self, path: &Path) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(meta) = datalib_store_meta::read_in(&mut tx).await? else {
            return Ok(());
        };
        if meta.schema_version > SCHEMA_VERSION {
            bail!(
                "{} was written by Datalib {} (schema {}), newer than this build \
                 (schema {SCHEMA_VERSION}); run that build or a later one",
                path.display(),
                meta.datalib_version,
                meta.schema_version
            );
        }
        for rung in LADDER.iter().filter(|m| m.version > meta.schema_version) {
            tracing::info!(
                version = rung.version,
                name = rung.name,
                "supervisor store: migrating"
            );
            (rung.apply)(&mut tx)
                .await
                .with_context(|| format!("migrate {} to v{}", path.display(), rung.version))?;
        }
        sql!(
            "dag.supervisor.update_schema_version",
            "UPDATE _datalib_meta SET value = ? WHERE key = 'schema_version'"
        )
        .query()
        .bind(SCHEMA_VERSION.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub fn listeners(&self) -> &Path {
        &self.listeners
    }

    pub(super) fn me(&self) -> &str {
        &self.me
    }

    /// After every commit this store makes, and never before one.
    pub(super) fn announce(&self, what: &str) {
        super::announce::announce(&self.listeners, &self.me, what);
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Moves whenever another connection commits a change; the loop's cue
    /// to read the intent again.
    pub async fn data_version(&self) -> Result<i64> {
        Ok(
            sql!("dag.supervisor.read_data_version", "PRAGMA data_version")
                .query_scalar()
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn open_request(&self, roots: &[String], by: &str) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let (now, tz_offset) = now_split();
        sql!(
            "dag.supervisor.insert_request",
            "INSERT INTO requests (id, roots, opened_by, opened_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?)"
        )
        .query()
        .bind(&id)
        .bind(serde_json::to_string(roots)?)
        .bind(by)
        .bind(now)
        .bind(tz_offset)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("request opened {id}"));
        Ok(id)
    }

    /// Asking, not doing: the loop stops the request's steps and closes it.
    /// Asking twice, or about a closed request, changes nothing.
    pub async fn request_stop(&self, id: &str, by: &str) -> Result<()> {
        let (now, _) = now_split();
        sql!(
            "dag.supervisor.mark_request_stop",
            "UPDATE requests SET stop_requested_by = ?, stop_requested_at_utc = ? \
             WHERE id = ? AND closed_at_utc IS NULL AND stop_requested_by IS NULL"
        )
        .query()
        .bind(by)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("stop asked {id}"));
        Ok(())
    }

    /// The loop's to call, and only the loop's.
    pub async fn close_request(
        &self,
        id: &str,
        outcome: RequestOutcome,
        failed_step: Option<&str>,
    ) -> Result<()> {
        let (now, _) = now_split();
        sql!(
            "dag.supervisor.close_request",
            "UPDATE requests SET closed_at_utc = ?, outcome = ?, failed_step = ? \
             WHERE id = ? AND closed_at_utc IS NULL"
        )
        .query()
        .bind(now)
        .bind(outcome.as_str())
        .bind(failed_step)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("request closed {id}"));
        Ok(())
    }

    pub async fn open_requests(&self) -> Result<Vec<RequestRow>> {
        let rows = OPEN_REQUESTS.query().fetch_all(&self.pool).await?;
        rows.iter().map(request_of).collect()
    }

    /// The open requests and the switches (step id → who turned it off),
    /// as one commit left them. Read apart, a stop and then a turn-on
    /// landing between the two reads looked like a request still open for
    /// a step turned back on, and the loop ran a stopped sync's step again.
    pub async fn requests_and_turned_off(
        &self,
    ) -> Result<(Vec<RequestRow>, BTreeMap<String, String>)> {
        let mut tx = self.pool.begin().await?;
        let rows = OPEN_REQUESTS.query().fetch_all(&mut *tx).await?;
        let turned_off = sql!(
            "dag.supervisor.mailbox_list_turned_off",
            "SELECT step, turned_off_by FROM turned_off"
        )
        .query()
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let requests = rows.iter().map(request_of).collect::<Result<_>>()?;
        let turned_off = turned_off
            .iter()
            .map(|r| Ok((r.try_get("step")?, r.try_get("turned_off_by")?)))
            .collect::<Result<_>>()?;
        Ok((requests, turned_off))
    }

    /// The open requests and then the newest closed ones, up to `limit`
    /// in all.
    pub async fn recent_requests(&self, limit: u32) -> Result<Vec<RequestRow>> {
        let rows = sql!(
            "dag.supervisor.list_recent_requests",
            "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
             FROM requests ORDER BY closed_at_utc IS NOT NULL, opened_at_utc DESC, id DESC \
             LIMIT ?"
        )
        .query()
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(request_of).collect()
    }

    pub async fn request(&self, id: &str) -> Result<Option<RequestRow>> {
        let row = sql!(
            "dag.supervisor.read_request",
            "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
             FROM requests WHERE id = ?"
        )
        .query()
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(request_of).transpose()
    }

    pub async fn turn_off(&self, step: &str, by: &str) -> Result<()> {
        let (now, tz_offset) = now_split();
        sql!("dag.supervisor.insert_turned_off", "INSERT INTO turned_off (step, turned_off_by, turned_off_at_utc, tz_offset) VALUES (?, ?, ?, ?) \
             ON CONFLICT(step) DO NOTHING").query()
        .bind(step)
        .bind(by)
        .bind(now)
        .bind(tz_offset)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("turned off {step}"));
        Ok(())
    }

    pub async fn turn_on(&self, step: &str) -> Result<()> {
        sql!(
            "dag.supervisor.delete_turned_off",
            "DELETE FROM turned_off WHERE step = ?"
        )
        .query()
        .bind(step)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("turned on {step}"));
        Ok(())
    }

    /// Carry whether a step is turned off over to its new id, for every
    /// step at or under `old`, now spelled `new`. Only the switch, which
    /// is a person's choice; what the loop learned about a step it learns
    /// again under the new id. A row the new id already has is left as
    /// it is. Returns how many rows moved.
    pub async fn rename_switches(&self, old: &str, new: &str) -> Result<u64> {
        let moved = sql!(
            "dag.supervisor.rename_turned_off_under",
            "UPDATE OR IGNORE turned_off SET step = ? || substr(step, ?) \
             WHERE step = ? OR substr(step, 1, ?) = ?"
        )
        .query()
        .bind(new)
        .bind(old.len() as i64 + 1)
        .bind(old)
        .bind(old.len() as i64 + 1)
        .bind(format!("{old}/"))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(moved)
    }

    /// Carry what the loop keeps per step over to the step's new id, for
    /// the one step `old` now called `new`: whether it is turned off, its
    /// last success and what it read then, its tree's version, and every
    /// other step's `reads` entry for that version. The run history
    /// keeps the id it ran under. A row the new id already has is left as
    /// it is. Returns how many rows moved.
    pub async fn rename_step(&self, old: &str, new: &str) -> Result<u64> {
        // `reads` is JSON keyed by input path, so a key is the id in quotes.
        let (old_key, new_key) = (format!("\"{old}\""), format!("\"{new}\""));
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut moved = 0;
        for (table, column) in [("turned_off", "step"), ("steps", "step"), ("sinks", "path")] {
            // Safe: `table` and `column` are literals from the list above.
            let rename = Sql::dynamic(
                sql_id!("dag.supervisor.rename_step_rows"),
                format!("UPDATE OR IGNORE {table} SET {column} = ? WHERE {column} = ?"),
            );
            moved += rename
                .query()
                .bind(new)
                .bind(old)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        sql!(
            "dag.supervisor.rename_step_in_reads",
            "UPDATE steps SET reads = replace(reads, ?, ?) WHERE instr(reads, ?) > 0"
        )
        .query()
        .bind(&old_key)
        .bind(&new_key)
        .bind(&old_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if moved > 0 {
            self.announce(&format!("renamed {old} to {new}"));
        }
        Ok(moved)
    }

    /// Carry what the loop keeps per step over to the step's new id, for
    /// every step under the group `old` now named `new`: whether it is
    /// turned off, its last success and what it read then, and its tree's
    /// version. The run history keeps the ids it ran under. A row the new
    /// id already has is left as it is. Returns how many rows moved.
    pub async fn rename_group_steps(&self, old: &str, new: &str) -> Result<u64> {
        let old_prefix = format!("{old}/");
        let new_prefix = format!("{new}/");
        // Both an input path and a step id: `reads` is JSON keyed by path.
        let (old_key, new_key) = (format!("\"{old_prefix}"), format!("\"{new_prefix}"));
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut moved = 0;
        for (table, column) in [("turned_off", "step"), ("steps", "step"), ("sinks", "path")] {
            // Safe: `table` and `column` are literals from the list above.
            let rename = Sql::dynamic(
                sql_id!("dag.supervisor.rename_group_step_rows"),
                format!(
                    "UPDATE OR IGNORE {table} SET {column} = ? || substr({column}, ?) \
                 WHERE substr({column}, 1, ?) = ?"
                ),
            );
            moved += rename
                .query()
                .bind(&new_prefix)
                .bind(old_prefix.len() as i64 + 1)
                .bind(old_prefix.len() as i64)
                .bind(&old_prefix)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        sql!(
            "dag.supervisor.rename_group_in_reads",
            "UPDATE steps SET reads = replace(reads, ?, ?) WHERE instr(reads, ?) > 0"
        )
        .query()
        .bind(&old_key)
        .bind(&new_key)
        .bind(&old_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if moved > 0 {
            self.announce(&format!("renamed {old}/ to {new}/"));
        }
        Ok(moved)
    }

    /// Step id → who turned it off.
    pub async fn turned_off(&self) -> Result<BTreeMap<String, String>> {
        let rows = sql!(
            "dag.supervisor.list_turned_off",
            "SELECT step, turned_off_by FROM turned_off"
        )
        .query()
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("step")?, r.try_get("turned_off_by")?)))
            .collect()
    }

    pub async fn open_job(&self, kind: JobKind, targets: &[String], by: &str) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let (now, tz_offset) = now_split();
        sql!(
            "dag.supervisor.insert_job",
            "INSERT INTO jobs (id, kind, targets, opened_by, opened_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?, ?)"
        )
        .query()
        .bind(&id)
        .bind(kind.as_str())
        .bind(serde_json::to_string(targets)?)
        .bind(by)
        .bind(now)
        .bind(tz_offset)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("job opened {id}"));
        Ok(id)
    }

    /// The loop's to call, and only the loop's. `error` is why it was not
    /// done; `None` when it was.
    pub async fn close_job(&self, id: &str, error: Option<&str>) -> Result<()> {
        let (now, _) = now_split();
        sql!(
            "dag.supervisor.close_job",
            "UPDATE jobs SET closed_at_utc = ?, error = ? WHERE id = ? AND closed_at_utc IS NULL"
        )
        .query()
        .bind(now)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("job closed {id}"));
        Ok(())
    }

    pub async fn open_jobs(&self) -> Result<Vec<JobRow>> {
        let rows = OPEN_JOBS.query().fetch_all(&self.pool).await?;
        rows.iter().map(wipe_of).collect()
    }

    /// Open and closed, oldest first.
    pub async fn jobs(&self) -> Result<Vec<JobRow>> {
        let rows = sql!(
            "dag.supervisor.list_jobs",
            "SELECT id, kind, targets, opened_by, closed_at_utc, error \
             FROM jobs ORDER BY opened_at_utc, id"
        )
        .query()
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(wipe_of).collect()
    }

    pub async fn job(&self, id: &str) -> Result<Option<JobRow>> {
        let row = sql!(
            "dag.supervisor.read_job",
            "SELECT id, kind, targets, opened_by, closed_at_utc, error FROM jobs WHERE id = ?"
        )
        .query()
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(wipe_of).transpose()
    }
}

impl Store {
    /// The loop's to call, before it runs the first step: every step the
    /// job runs its verb for, waiting.
    pub async fn begin_job_steps(&self, wipe: &str, steps: &[&str]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sql!(
            "dag.supervisor.delete_job_steps",
            "DELETE FROM job_steps WHERE job_id = ?"
        )
        .query()
        .bind(wipe)
        .execute(&mut *tx)
        .await?;
        for (ord, step) in steps.iter().enumerate() {
            sql!(
                "dag.supervisor.insert_job_step",
                "INSERT INTO job_steps (job_id, ord, step, state) VALUES (?, ?, ?, ?)"
            )
            .query()
            .bind(wipe)
            .bind(ord as i64)
            .bind(step)
            .bind(PassState::Waiting.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.announce(&format!("{JOB_MOVED} {wipe}"));
        Ok(())
    }

    /// The loop's to call, as each step it runs moves.
    pub async fn set_job_step(
        &self,
        wipe: &str,
        ord: usize,
        state: PassState,
        error: Option<&str>,
        bytes: Option<StoreBytes>,
    ) -> Result<()> {
        sql!(
            "dag.supervisor.update_job_step",
            "UPDATE job_steps SET state = ?, error = ?, before_bytes = ?, after_bytes = ? \
             WHERE job_id = ? AND ord = ?"
        )
        .query()
        .bind(state.as_str())
        .bind(error)
        .bind(bytes.map(|b| b.before as i64))
        .bind(bytes.map(|b| b.after as i64))
        .bind(wipe)
        .bind(ord as i64)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("{JOB_MOVED} {wipe}"));
        Ok(())
    }

    /// In the order the job runs them. A state this build does not know
    /// reads as waiting.
    pub async fn job_steps(&self, wipe: &str) -> Result<Vec<PassRow>> {
        let rows = sql!(
            "dag.supervisor.list_job_steps",
            "SELECT step, state, error, before_bytes, after_bytes FROM job_steps \
             WHERE job_id = ? ORDER BY ord"
        )
        .query()
        .bind(wipe)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let state: String = r.try_get("state")?;
                let before: Option<i64> = r.try_get("before_bytes")?;
                let after: Option<i64> = r.try_get("after_bytes")?;
                Ok(PassRow {
                    step: r.try_get("step")?,
                    state: PassState::parse(&state).unwrap_or(PassState::Waiting),
                    error: r.try_get("error")?,
                    store_bytes: before.zip(after).map(|(b, a)| StoreBytes {
                        before: b as u64,
                        after: a as u64,
                    }),
                })
            })
            .collect()
    }

    /// The newest compaction or collection opened at or after
    /// `since_utc`, open or closed.
    pub async fn latest_shrink_since(&self, since_utc: &str) -> Result<Option<JobRow>> {
        self.latest_since(&[JobKind::Compact, JobKind::Collect], since_utc)
            .await
    }

    /// The newest prepare pass opened at or after `since_utc`, open or
    /// closed.
    pub async fn latest_prepare_since(&self, since_utc: &str) -> Result<Option<JobRow>> {
        self.latest_since(&[JobKind::Prepare], since_utc).await
    }

    async fn latest_since(&self, kinds: &[JobKind], since_utc: &str) -> Result<Option<JobRow>> {
        let kinds: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let row = sql!(
            "dag.supervisor.find_latest_job_since",
            "SELECT id, kind, targets, opened_by, closed_at_utc, error FROM jobs \
             WHERE kind IN (SELECT value FROM json_each(?)) AND opened_at_utc >= ? \
             ORDER BY opened_at_utc DESC, id DESC LIMIT 1"
        )
        .query()
        .bind(serde_json::to_string(&kinds)?)
        .bind(since_utc)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(wipe_of).transpose()
    }
}

/// What the store announces when a job's steps move, ahead of the job's
/// id.
const JOB_MOVED: &str = "job moved";

/// The clock the store stamps with, as UTC text: what
/// [`Store::latest_shrink_since`] compares against.
pub fn now_utc() -> String {
    now_split().0
}

fn wipe_of(r: &sqlx::sqlite::SqliteRow) -> Result<JobRow> {
    let kind: String = r.try_get("kind")?;
    let targets: String = r.try_get("targets")?;
    let closed_at: Option<String> = r.try_get("closed_at_utc")?;
    let error: Option<String> = r.try_get("error")?;
    Ok(JobRow {
        id: r.try_get("id")?,
        kind: JobKind::parse(&kind),
        targets: serde_json::from_str(&targets).context("a wipe's targets")?,
        opened_by: r.try_get("opened_by")?,
        closed: closed_at.map(|_| error),
    })
}

fn request_of(r: &sqlx::sqlite::SqliteRow) -> Result<RequestRow> {
    let roots: String = r.try_get("roots")?;
    let closed_at: Option<String> = r.try_get("closed_at_utc")?;
    let outcome: Option<String> = r.try_get("outcome")?;
    Ok(RequestRow {
        id: r.try_get("id")?,
        roots: serde_json::from_str(&roots).context("a request's roots")?,
        opened_by: r.try_get("opened_by")?,
        stop_requested_by: r.try_get("stop_requested_by")?,
        closed: closed_at.map(|_| outcome.as_deref().and_then(RequestOutcome::parse)),
        failed_step: r.try_get("failed_step")?,
    })
}

fn now_split() -> (String, String) {
    datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset()
}

fn options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // Rollback journal, the mode every existing file is in; the
        // plain-SQLite engine switches a file it is asked to.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .busy_timeout(BUSY_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    async fn columns(store: &Store, table: &str) -> Vec<String> {
        // Safe: `table` is a literal in a test.
        let pragma = Sql::dynamic(test_sql_id!(), format!("PRAGMA table_info({table})"));
        pragma
            .query()
            .fetch_all(store.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| r.get("name"))
            .collect()
    }

    /// `steps` as two releases left it: 0.38, the first with this store,
    /// and 0.55, the last before the ladder, which also kept
    /// `launch_passes`. Copied from those tags.
    const STEPS_AT_0_38: &str = "CREATE TABLE steps (step TEXT PRIMARY KEY, \
        succeeded INTEGER NOT NULL, fingerprint TEXT NOT NULL, reads TEXT NOT NULL, \
        last_run_id TEXT, last_started_at_utc TEXT, last_finished_at_utc TEXT, \
        last_status TEXT, last_attempts INTEGER, last_error TEXT, last_success_at_utc TEXT, \
        tz_offset TEXT, state TEXT, state_detail TEXT, turned_off_by TEXT, requests TEXT)";
    const STEPS_AT_0_55: &str = "CREATE TABLE steps (step TEXT PRIMARY KEY, \
        succeeded INTEGER NOT NULL, fingerprint TEXT NOT NULL, reads TEXT NOT NULL, \
        last_run_id TEXT, last_started_at_utc TEXT, last_finished_at_utc TEXT, \
        last_status TEXT, last_attempts INTEGER, last_error TEXT, last_failure_kind TEXT, \
        last_success_at_utc TEXT, tz_offset TEXT, state TEXT, state_detail TEXT, \
        turned_off_by TEXT, requests TEXT, needs_rerun INTEGER NOT NULL DEFAULT 0)";
    const LAUNCH_PASSES: &str = "CREATE TABLE launch_passes (build TEXT PRIMARY KEY, \
        finished_at_utc TEXT NOT NULL, tz_offset TEXT NOT NULL)";

    /// A store as a release before the ladder left it reaches this
    /// build's shape at its first open: every step's `steps` row and every
    /// switch a person set carry over, `steps` has exactly the columns a
    /// new store's has, and `launch_passes` is gone.
    #[tokio::test]
    async fn a_store_from_before_the_ladder_climbs_to_this_builds_shape() {
        let fresh = tempfile::tempdir().unwrap();
        let store = Store::open(fresh.path()).await.unwrap();
        let want = columns(&store, "steps").await;
        store.close().await;

        for old in [&[STEPS_AT_0_38][..], &[STEPS_AT_0_55, LAUNCH_PASSES]] {
            let root = tempfile::tempdir().unwrap();
            let path = datalib_runtime::layout::supervisor_db(root.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(options(&path))
                .await
                .unwrap();
            for stmt in old.iter().chain(DDL.iter()) {
                // Safe: every statement is a constant of this file.
                Sql::dynamic(test_sql_id!(), *stmt)
                    .query()
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            test_sql!(
                "INSERT INTO steps (step, succeeded, fingerprint, reads, last_status) \
                 VALUES ('a/ingest', 1, 'f', '{}', 'succeeded')"
            )
            .query()
            .execute(&pool)
            .await
            .unwrap();
            test_sql!("INSERT INTO turned_off VALUES ('a/render_markdown', 'thad', 't', 'z')")
                .query()
                .execute(&pool)
                .await
                .unwrap();
            datalib_store_meta::write(
                &pool,
                datalib_store_meta::StoreKind::Supervisor,
                &datalib_store_meta::Shape {
                    tables: "a release's".into(),
                    indexes: None,
                    versions: datalib_store_meta::Versions {
                        schema: 5,
                        shared: 0,
                    },
                },
            )
            .await
            .unwrap();
            pool.close().await;

            for _ in 0..2 {
                let store = Store::open(root.path()).await.unwrap();
                assert_eq!(columns(&store, "steps").await, want);
                let record = store.load_record().await.unwrap();
                assert!(record.steps["a/ingest"].succeeded, "{record:?}");
                assert_eq!(
                    store
                        .turned_off()
                        .await
                        .unwrap()
                        .into_keys()
                        .collect::<Vec<_>>(),
                    ["a/render_markdown"]
                );
                let tables: Vec<String> = test_sql!(
                    "SELECT name FROM sqlite_master WHERE name IN ('launch_passes', 'steps_v5')"
                )
                .query_scalar()
                .fetch_all(store.pool())
                .await
                .unwrap();
                assert!(tables.is_empty(), "{tables:?}");
                let meta = datalib_store_meta::read(store.pool())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(meta.schema_version, SCHEMA_VERSION);
                store.close().await;
            }
        }
    }

    /// A store at version 6 kept its resets, purges and free-disk jobs
    /// in `wipes` and `wipe_steps`. At its first open they are `jobs` and
    /// `job_steps`, every row with them, and an open one is still read as
    /// open: a reset asked for just before the upgrade is still carried
    /// out.
    #[tokio::test]
    async fn a_store_with_wipes_keeps_them_as_jobs() {
        let root = tempfile::tempdir().unwrap();
        let path = datalib_runtime::layout::supervisor_db(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options(&path))
            .await
            .unwrap();
        test_sql!(
            "CREATE TABLE wipes (id TEXT PRIMARY KEY, kind TEXT NOT NULL, \
             targets TEXT NOT NULL, opened_by TEXT NOT NULL, opened_at_utc TEXT NOT NULL, \
             tz_offset TEXT NOT NULL, closed_at_utc TEXT, error TEXT); \
             CREATE TABLE wipe_steps (wipe_id TEXT NOT NULL, ord INTEGER NOT NULL, \
             step TEXT NOT NULL, state TEXT NOT NULL, error TEXT, before_bytes INTEGER, \
             after_bytes INTEGER, PRIMARY KEY (wipe_id, ord)); \
             CREATE INDEX wipes_open ON wipes (closed_at_utc, opened_at_utc, id); \
             INSERT INTO wipes VALUES ('w1', 'reset', '[\"a/ingest\"]', 'ui', 't', 'z', \
             NULL, NULL); \
             INSERT INTO wipes VALUES ('w2', 'collect', '[]', 'ui', 't', 'z', 't', NULL); \
             INSERT INTO wipe_steps VALUES ('w2', 0, 'a/ingest', 'done', NULL, 10, 4);"
        )
        .raw()
        .execute(&pool)
        .await
        .unwrap();
        datalib_store_meta::write(
            &pool,
            datalib_store_meta::StoreKind::Supervisor,
            &datalib_store_meta::Shape {
                tables: "a release's".into(),
                indexes: None,
                versions: datalib_store_meta::Versions {
                    schema: 6,
                    shared: 0,
                },
            },
        )
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(root.path()).await.unwrap();
        let open = store.open_jobs().await.unwrap();
        assert_eq!(open.len(), 1, "{open:?}");
        assert_eq!(
            (open[0].id.as_str(), open[0].kind),
            ("w1", Some(JobKind::Reset))
        );
        assert_eq!(open[0].targets, ["a/ingest"]);
        let steps = store.job_steps("w2").await.unwrap();
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].state, PassState::Done);
        let left: Vec<String> = test_sql!(
            "SELECT name FROM sqlite_master WHERE name IN ('wipes', 'wipe_steps', 'wipes_open')"
        )
        .query_scalar()
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(left.is_empty(), "{left:?}");
        store.close().await;
    }

    /// `requests`, `jobs` and `invocations` keep every row they were
    /// given, and the loop reads only the open ones. A store from before
    /// their indexes gets them at its next open, and each read seeks the
    /// open rows rather than reading the whole table.
    #[tokio::test]
    async fn the_open_rows_are_read_by_an_index_an_old_store_gains() {
        use crate::supervisor::record::{CLOSE_ABANDONED_INVOCATIONS, RUNNING_INVOCATIONS};
        const NAMES: [&str; 3] = ["requests_open", "jobs_open", "invocations_open"];
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        for name in NAMES {
            // Safe: `name` is one of the three literals above.
            let drop = Sql::dynamic(test_sql_id!(), format!("DROP INDEX {name}"));
            drop.query().execute(store.pool()).await.unwrap();
        }
        store.close().await;

        let store = Store::open(root.path()).await.unwrap();
        for (sql, seek) in [
            (OPEN_REQUESTS, "USING INDEX requests_open (closed_at_utc=?)"),
            (OPEN_JOBS, "USING INDEX jobs_open (closed_at_utc=?)"),
            (
                RUNNING_INVOCATIONS,
                "USING INDEX invocations_open (outcome=?)",
            ),
            (
                CLOSE_ABANDONED_INVOCATIONS,
                "USING INDEX invocations_open (outcome=?)",
            ),
        ] {
            // Safe: each `sql` is one of this module's constants.
            let explain = Sql::dynamic(test_sql_id!(), format!("EXPLAIN QUERY PLAN {sql}"));
            let plan: Vec<String> = explain
                .query()
                .fetch_all(store.pool())
                .await
                .unwrap()
                .iter()
                .map(|r| r.get("detail"))
                .collect();
            assert!(
                plan.iter()
                    .any(|d| d.starts_with("SEARCH") && d.ends_with(seek)),
                "{sql}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|d| d.contains("TEMP B-TREE")),
                "{sql}: {plan:?}"
            );
        }
        store.close().await;
    }

    /// A step turned off under the index's old group id stays off under
    /// the new one, and a step merely sharing the prefix is not moved.
    #[tokio::test]
    async fn renaming_a_group_carries_its_steps_rows_over() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        store
            .turn_off("unified_index/embedding_map", "test")
            .await
            .unwrap();
        store
            .turn_off("unified_index_extra/x", "test")
            .await
            .unwrap();
        test_sql!(
            "INSERT INTO steps (step, succeeded, fingerprint, reads) \
             VALUES ('unified_index/embedding_map', 1, 'f', ?)"
        )
        .query()
        .bind(r#"{"unified_index/qmd_aggregator":"v1"}"#)
        .execute(store.pool())
        .await
        .unwrap();
        test_sql!("INSERT INTO sinks (path, version) VALUES ('unified_index/grid_index', 'v2')")
            .query()
            .execute(store.pool())
            .await
            .unwrap();

        let moved = store
            .rename_group_steps("unified_index", "unified_grid_and_search_index")
            .await
            .unwrap();
        assert_eq!(moved, 3);
        let off: Vec<String> = store.turned_off().await.unwrap().into_keys().collect();
        assert_eq!(
            off,
            [
                "unified_grid_and_search_index/embedding_map",
                "unified_index_extra/x"
            ]
        );
        let (step, reads): (String, String) = test_sql!("SELECT step, reads FROM steps")
            .query_as()
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(step, "unified_grid_and_search_index/embedding_map");
        assert_eq!(
            reads,
            r#"{"unified_grid_and_search_index/qmd_aggregator":"v1"}"#
        );
        let sink: String = test_sql!("SELECT path FROM sinks")
            .query_scalar()
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(sink, "unified_grid_and_search_index/grid_index");
        assert_eq!(
            store
                .rename_group_steps("unified_index", "unified_grid_and_search_index")
                .await
                .unwrap(),
            0
        );
        store.close().await;
    }

    /// A renamed step keeps its switch, its last success and its tree's
    /// version, a reader's `reads` entry for that version follows it, and a step
    /// whose id merely starts with the old one is not moved.
    #[tokio::test]
    async fn renaming_a_step_carries_its_rows_over() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let (old, new) = ("u/grid_index", "u/unified_grid_and_search_index");
        store.turn_off(old, "test").await.unwrap();
        store.turn_off("u/grid_index_extra", "test").await.unwrap();
        for (step, reads) in [
            (old, r#"{"a/render_markdown":"v1"}"#),
            ("a/embed", r#"{"u/grid_index":"v2"}"#),
        ] {
            test_sql!(
                "INSERT INTO steps (step, succeeded, fingerprint, reads) VALUES (?, 1, 'f', ?)"
            )
            .query()
            .bind(step)
            .bind(reads)
            .execute(store.pool())
            .await
            .unwrap();
        }
        test_sql!("INSERT INTO sinks (path, version) VALUES (?, 'v2')")
            .query()
            .bind(old)
            .execute(store.pool())
            .await
            .unwrap();

        assert_eq!(store.rename_step(old, new).await.unwrap(), 3);
        let off: Vec<String> = store.turned_off().await.unwrap().into_keys().collect();
        assert_eq!(off, ["u/grid_index_extra", new]);
        let steps: Vec<(String, String)> = test_sql!("SELECT step, reads FROM steps ORDER BY step")
            .query_as()
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(
            steps,
            [
                ("a/embed".to_string(), format!(r#"{{"{new}":"v2"}}"#)),
                (new.to_string(), r#"{"a/render_markdown":"v1"}"#.to_string()),
            ]
        );
        let sink: String = test_sql!("SELECT path FROM sinks")
            .query_scalar()
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(sink, new);
        assert_eq!(store.rename_step(old, new).await.unwrap(), 0);
        store.close().await;
    }

    /// An id whose case changed keeps a person's switch: `Email/embed`
    /// turned off is `email/embed` turned off, and nothing else moves.
    #[tokio::test]
    async fn a_renamed_spelling_keeps_its_switches() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        for step in ["Email/embed", "Email", "g/Raw", "Emailer/x"] {
            store.turn_off(step, "thad").await.unwrap();
        }
        assert_eq!(store.rename_switches("Email", "email").await.unwrap(), 2);
        assert_eq!(store.rename_switches("g/Raw", "g/raw").await.unwrap(), 1);
        let off: Vec<String> = store.turned_off().await.unwrap().into_keys().collect();
        assert_eq!(off, ["Emailer/x", "email", "email/embed", "g/raw"]);
        store.close().await;
    }

    #[tokio::test]
    async fn a_request_opens_and_closes_once() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let id = store
            .open_request(&["mail/ingest".to_string()], "cli")
            .await
            .unwrap();
        let open = store.open_requests().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].roots, ["mail/ingest"]);
        assert_eq!(open[0].closed, None);

        store
            .close_request(&id, RequestOutcome::Failed, Some("mail/ingest"))
            .await
            .unwrap();
        store
            .close_request(&id, RequestOutcome::Done, None)
            .await
            .unwrap();
        let row = store.request(&id).await.unwrap().unwrap();
        assert_eq!(
            row.closed,
            Some(Some(RequestOutcome::Failed)),
            "closed once"
        );
        assert_eq!(row.failed_step.as_deref(), Some("mail/ingest"));
        assert!(store.open_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stop_is_asked_for_once_and_not_after_the_close() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let id = store
            .open_request(&["a/ingest".into()], "ui")
            .await
            .unwrap();
        store.request_stop(&id, "claude").await.unwrap();
        store.request_stop(&id, "ui").await.unwrap();
        let row = store.request(&id).await.unwrap().unwrap();
        assert_eq!(row.stop_requested_by.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn a_turn_off_remembers_who_and_a_turn_on_lifts_it() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        store.turn_off("a/ingest", "ui").await.unwrap();
        store.turn_off("a/ingest", "claude").await.unwrap();
        assert_eq!(
            store.turned_off().await.unwrap(),
            BTreeMap::from([("a/ingest".to_string(), "ui".to_string())])
        );
        store.turn_on("a/ingest").await.unwrap();
        assert!(store.turned_off().await.unwrap().is_empty());
    }

    /// The loop's cue: a row another process writes moves this
    /// connection's `data_version`, and its own write does not.
    #[tokio::test]
    async fn another_processs_write_moves_the_data_version() {
        let root = tempfile::tempdir().unwrap();
        let loop_side = Store::open(root.path()).await.unwrap();
        let other = Store::open(root.path()).await.unwrap();
        let before = loop_side.data_version().await.unwrap();
        loop_side.turn_off("x/ingest", "loop").await.unwrap();
        assert_eq!(loop_side.data_version().await.unwrap(), before);
        other
            .open_request(&["a/ingest".into()], "ui")
            .await
            .unwrap();
        assert_ne!(loop_side.data_version().await.unwrap(), before);
    }

    #[tokio::test]
    async fn a_store_a_newer_build_wrote_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        test_sql!("UPDATE _datalib_meta SET value = '99' WHERE key = 'schema_version'")
            .query()
            .execute(&store.pool)
            .await
            .unwrap();
        store.close().await;
        let err = Store::open(root.path()).await.err().expect("refused");
        assert!(
            format!("{err:#}").contains("newer than this build"),
            "{err:#}"
        );
    }

    /// strum and serde spell these independently.
    #[test]
    fn outcome_as_str_matches_the_serde_spelling() {
        for &v in RequestOutcome::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(RequestOutcome::parse(v.as_str()), Some(v));
        }
        for &v in JobKind::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(JobKind::parse(v.as_str()), Some(v));
        }
    }

    #[tokio::test]
    async fn a_wipe_opens_and_closes_once_with_why_it_was_not_done() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let id = store
            .open_job(JobKind::Purge, &["gone".into()], "ui")
            .await
            .unwrap();
        let open = store.open_jobs().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].kind, open[0].targets.as_slice(), &open[0].closed),
            (Some(JobKind::Purge), &["gone".to_string()][..], &None)
        );
        store.close_job(&id, Some("refused")).await.unwrap();
        store.close_job(&id, None).await.unwrap();
        let row = store.job(&id).await.unwrap().unwrap();
        assert_eq!(row.closed, Some(Some("refused".into())), "closed once");
        assert!(store.open_jobs().await.unwrap().is_empty());
    }
}
