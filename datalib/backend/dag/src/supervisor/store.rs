//! `system/supervisor.sqlite`: the mailbox, and the loop's record. Anyone
//! writes intent into it — open a request, ask for one to stop, pause or
//! resume a step — and the one process running the loop reads it and
//! writes back how each request ended, and what each step did
//! (`record.rs`). Plain SQLite in rollback-journal mode, several writing
//! processes at once; its schema only grows, because two builds may share
//! it. Every commit is announced (`announce.rs`), which is how the loop
//! and the server hear of it. `docs/dev/plans/supervisor.md` §2.7–§2.8.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use strum::{EnumString, IntoStaticStr, VariantArray};

/// Where this build's tables stand. A store at a higher version was
/// written by a newer build, whose columns this one would not fill.
const SCHEMA_VERSION: u32 = 3;

const DDL: [&str; 2] = [
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
    "CREATE TABLE IF NOT EXISTS pauses (
        step TEXT PRIMARY KEY,
        paused_by TEXT NOT NULL,
        paused_at_utc TEXT NOT NULL,
        tz_offset TEXT NOT NULL
    )",
];

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
            listeners: super::announce::listeners_dir(data_root),
            me: format!(
                "store-{}-{}",
                std::process::id(),
                NEXT_STORE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        };
        store.refuse_if_newer(&path).await?;
        let ddl = || DDL.into_iter().chain(super::record::DDL);
        for stmt in ddl() {
            sqlx::query(stmt).execute(&store.pool).await?;
        }
        store.add_missing_columns().await?;
        datalib_store_meta::write(
            &store.pool,
            datalib_store_meta::StoreKind::Supervisor,
            &datalib_store_meta::schema_hash(ddl()),
            SCHEMA_VERSION,
        )
        .await?;
        store.announce("store opened");
        Ok(store)
    }

    /// `CREATE TABLE IF NOT EXISTS` leaves a table an older build made as
    /// it was, so a column added since reaches it here.
    async fn add_missing_columns(&self) -> Result<()> {
        for (table, column, decl) in super::record::ADDED_COLUMNS {
            // Safe: `table` is a literal from `ADDED_COLUMNS`.
            let pragma = sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})"));
            let have: Vec<String> = sqlx::query(pragma)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.try_get("name"))
                .collect::<Result<_, _>>()?;
            if !have.iter().any(|c| c == column) {
                // Safe: every name here is a literal from `ADDED_COLUMNS`.
                let alter =
                    sqlx::AssertSqlSafe(format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"));
                sqlx::query(alter).execute(&self.pool).await?;
            }
        }
        Ok(())
    }

    async fn refuse_if_newer(&self, path: &Path) -> Result<()> {
        if let Some(meta) = datalib_store_meta::read(&self.pool).await? {
            if meta.schema_version > SCHEMA_VERSION {
                bail!(
                    "{} was written by datalib {} (schema {}), newer than this build \
                     (schema {SCHEMA_VERSION}); run that build or a later one",
                    path.display(),
                    meta.datalib_version,
                    meta.schema_version
                );
            }
        }
        Ok(())
    }

    pub(super) fn pool(&self) -> &SqlitePool {
        &self.pool
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
        Ok(sqlx::query_scalar("PRAGMA data_version")
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn open_request(&self, roots: &[String], by: &str) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let (now, tz_offset) = now_split();
        sqlx::query(
            "INSERT INTO requests (id, roots, opened_by, opened_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?)",
        )
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
        sqlx::query(
            "UPDATE requests SET stop_requested_by = ?, stop_requested_at_utc = ? \
             WHERE id = ? AND closed_at_utc IS NULL AND stop_requested_by IS NULL",
        )
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
        sqlx::query(
            "UPDATE requests SET closed_at_utc = ?, outcome = ?, failed_step = ? \
             WHERE id = ? AND closed_at_utc IS NULL",
        )
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
        let rows = sqlx::query(
            "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
             FROM requests WHERE closed_at_utc IS NULL ORDER BY opened_at_utc, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(request_of).collect()
    }

    /// The open requests and then the newest closed ones, up to `limit`
    /// in all.
    pub async fn recent_requests(&self, limit: u32) -> Result<Vec<RequestRow>> {
        let rows = sqlx::query(
            "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
             FROM requests ORDER BY closed_at_utc IS NOT NULL, opened_at_utc DESC, id DESC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(request_of).collect()
    }

    pub async fn request(&self, id: &str) -> Result<Option<RequestRow>> {
        let row = sqlx::query(
            "SELECT id, roots, opened_by, stop_requested_by, closed_at_utc, outcome, failed_step \
             FROM requests WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(request_of).transpose()
    }

    pub async fn pause(&self, step: &str, by: &str) -> Result<()> {
        let (now, tz_offset) = now_split();
        sqlx::query(
            "INSERT INTO pauses (step, paused_by, paused_at_utc, tz_offset) VALUES (?, ?, ?, ?) \
             ON CONFLICT(step) DO NOTHING",
        )
        .bind(step)
        .bind(by)
        .bind(now)
        .bind(tz_offset)
        .execute(&self.pool)
        .await?;
        self.announce(&format!("paused {step}"));
        Ok(())
    }

    pub async fn resume(&self, step: &str) -> Result<()> {
        sqlx::query("DELETE FROM pauses WHERE step = ?")
            .bind(step)
            .execute(&self.pool)
            .await?;
        self.announce(&format!("resumed {step}"));
        Ok(())
    }

    /// Step id → who paused it.
    pub async fn paused(&self) -> Result<BTreeMap<String, String>> {
        let rows = sqlx::query("SELECT step, paused_by FROM pauses")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("step")?, r.try_get("paused_by")?)))
            .collect()
    }
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
    // The plain-SQLite engine, not doltlite's: the URI parameter has to go
    // in through `filename`, which sqlx hands to SQLite untouched (its URL
    // parser rejects a parameter it does not know). As the run store does.
    let escaped = path
        .display()
        .to_string()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    SqliteConnectOptions::new()
        .filename(format!("file:{escaped}?doltlite_engine=sqlite"))
        .create_if_missing(true)
        // What the plain-SQLite engine really does: asked for WAL it
        // answers `wal` and stays in rollback-journal mode.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .busy_timeout(BUSY_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn a_pause_remembers_who_and_a_resume_lifts_it() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        store.pause("a/ingest", "ui").await.unwrap();
        store.pause("a/ingest", "claude").await.unwrap();
        assert_eq!(
            store.paused().await.unwrap(),
            BTreeMap::from([("a/ingest".to_string(), "ui".to_string())])
        );
        store.resume("a/ingest").await.unwrap();
        assert!(store.paused().await.unwrap().is_empty());
    }

    /// The loop's cue: a row another process writes moves this
    /// connection's `data_version`, and its own write does not.
    #[tokio::test]
    async fn another_processs_write_moves_the_data_version() {
        let root = tempfile::tempdir().unwrap();
        let loop_side = Store::open(root.path()).await.unwrap();
        let other = Store::open(root.path()).await.unwrap();
        let before = loop_side.data_version().await.unwrap();
        loop_side.pause("x/ingest", "loop").await.unwrap();
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
        sqlx::query("UPDATE _datalib_meta SET value = '99' WHERE key = 'schema_version'")
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
    }
}
