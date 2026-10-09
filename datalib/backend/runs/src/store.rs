//! Writing the store, and reading it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use datalib_flock::FileLock;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::{is_terminal, runs_path, LiveState, Retention, INDEXES, SCHEMA_VERSION};
use app_schema::runs::{
    LogRow, MetricRow, MetricSampleRow, Process, ProcessRow, RunRow, StepRunRow, StorePart,
};

/// How often the writer thread flushes. 200ms is under the threshold
/// where a progress display reads as laggy, and far above the cost of
/// the write (~0.3ms per row on a plain-SQLite file, measured).
const FLUSH_EVERY: Duration = Duration::from_millis(200);

/// How far back a snapshot reads the `queued` gauge, for an estimate of
/// when a step's queue empties. Long enough that one slow page does not
/// swing it, short enough that it follows a step that changes pace.
pub const QUEUE_WINDOW: Duration = Duration::from_secs(2 * 60);

/// The floor between two samples of one metric series. A series that
/// changes every flush would otherwise write five rows a second for as
/// long as the step runs; a rate drawn from five-second samples is the
/// same rate.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// How often a process-scoped writer re-applies retention to the lines
/// outside any run. A run prunes them when it starts; a server that
/// runs for weeks between syncs has to do it for itself.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// How long a writer waits for the other process's write lock before
/// SQLite hands it `SQLITE_BUSY` and its batch is lost. Set here rather
/// than left to sqlx's default, because the number is a decision: a
/// batch holds the lock for milliseconds, so ten seconds is far past
/// any honest wait, and a writer that has spent them is better off
/// saying so than growing its buffer behind a silent one.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a failed flush waits before its one retry.
const RETRY_AFTER: Duration = Duration::from_millis(50);

/// How long a process waits for another's open to finish before
/// deciding about the file for itself. An open takes milliseconds, so
/// reaching this means the holder is wedged — and a log store is not
/// worth holding a run up for.
const OPEN_LOCK_WAIT: Duration = Duration::from_secs(30);
const OPEN_LOCK_POLL: Duration = Duration::from_millis(20);

fn options(path: &Path, create: bool) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(create)
        // What the plain-SQLite engine really does: asked for WAL it
        // answers `wal` and stays in rollback-journal mode, so a reader
        // waits out a writer's commit (`BUSY_TIMEOUT`).
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        // Nothing here is load-bearing, so nothing is worth an fsync. A
        // file torn by a power cut is emptied and remade on the next
        // open (see `open_or_recreate`).
        .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
        .busy_timeout(BUSY_TIMEOUT)
}

pub async fn open_or_create(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(sqlx::Error::Io)?;
    }
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options(path, true))
        .await
}

/// Open an existing store read-only-ish. Never creates: a reader that
/// created the file would race the runner for which engine claims it.
pub(crate) async fn open_existing(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options(path, false))
        .await
}

/// Where the claim on *deciding about* the store lives. Not one of the
/// store's own sidecars.
fn open_lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".open-lock");
    PathBuf::from(p)
}

/// Hold the right to decide what happens to the file, for as long as
/// the returned lock lives. [`open_or_recreate`] reads the file and then
/// may empty it; two processes doing that at once would each find the
/// old version, and the second would empty the store the first had
/// already remade and begun writing.
///
/// A lock that cannot be taken is not worth failing an open over: the
/// store is not load-bearing, and the window it guards is one process
/// remaking the file.
async fn hold_open_lock(path: &Path) -> Option<FileLock> {
    let lock = open_lock_path(path);
    let deadline = Instant::now() + OPEN_LOCK_WAIT;
    loop {
        match FileLock::acquire(&lock) {
            Ok(held) => return Some(held),
            Err(e) if e.is_held() && Instant::now() < deadline => {
                tokio::time::sleep(OPEN_LOCK_POLL).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "run store: opening without the open lock");
                return None;
            }
        }
    }
}

/// Open the store and make sure its schema is there, emptying a file
/// that will not read or was written by another schema version, after
/// keeping a copy of it. `synchronous=Off` means an OS crash can leave
/// an unreadable file behind, and losing old logs is a better outcome
/// than a run that refuses to start.
async fn open_or_recreate(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let _deciding = hold_open_lock(path).await;
    let why = match open_or_create(path).await {
        Ok(pool) => match schema_matches(&pool).await {
            Ok(true) => {
                write_meta(&pool).await?;
                return Ok(pool);
            }
            Ok(false) => {
                pool.close().await;
                "written by another schema version".to_string()
            }
            Err(e) => {
                pool.close().await;
                e.to_string()
            }
        },
        Err(e) => e.to_string(),
    };
    tracing::warn!(path = %path.display(), why, "run store: emptying the file");
    let emptying = emptying_of(path)?;
    keep_a_copy(path, &emptying).await;
    empty_in_place(path, &emptying).await?;
    let pool = open_or_create(path).await?;
    install_schema(&pool).await?;
    write_meta(&pool).await?;
    Ok(pool)
}

/// How a file this build will not use is emptied. Never by deleting
/// it: a reader that opened it first — `datalib-http`'s reads take no
/// lock — stays on the old inode, where nobody holds a lock, so it
/// reads the new file's journal as a crashed writer's, rolls it back
/// and deletes it, and the new writer's commit fails.
#[derive(Debug, PartialEq, Eq)]
enum Emptying {
    /// SQLite's own reset (`SQLITE_DBCONFIG_RESET_DATABASE`, then
    /// `VACUUM`), which takes the file's lock like any write and works
    /// on a corrupt page.
    Reset,
    /// Cut the file to nothing, and its journal with it. Doltlite reads
    /// a file without SQLite's header as its own format and refuses the
    /// reset, and it refuses every other connection's use of the file
    /// the same way, so none can be reading or writing it.
    Truncate,
}

fn emptying_for(head: &[u8]) -> Emptying {
    if head.starts_with(b"SQLite format 3\0") {
        Emptying::Reset
    } else {
        Emptying::Truncate
    }
}

fn emptying_of(path: &Path) -> Result<Emptying, sqlx::Error> {
    use std::io::Read;
    let mut head = Vec::with_capacity(16);
    std::fs::File::open(path)
        .and_then(|f| f.take(16).read_to_end(&mut head))
        .map_err(sqlx::Error::Io)?;
    Ok(emptying_for(&head))
}

/// Where a file about to be emptied is kept: beside it, named for the
/// moment it was set aside, in UTC. Not `runs.sqlite…`, which the app's
/// file watcher reads as a write to the store.
fn backup_path(path: &Path, now: &datalib_time::IsoOffsetTimestamp) -> PathBuf {
    path.with_file_name(format!(
        "runs.bak_{}.sqlite",
        now.inner().naive_utc().format("%Y%m%dT%H%M%SZ")
    ))
}

/// Copied, never renamed: a reader that opened the file keeps it under
/// its new name but looks for its journal by the old one, which is the
/// trap [`Emptying`] describes. A copy that fails costs the old lines
/// and nothing else.
async fn keep_a_copy(path: &Path, emptying: &Emptying) {
    let to = backup_path(path, &datalib_time::IsoOffsetTimestamp::now_local());
    let kept = match emptying {
        Emptying::Reset => vacuum_into(path, &to).await.map_err(|e| e.to_string()),
        Emptying::Truncate => std::fs::copy(path, &to)
            .map(|_| ())
            .map_err(|e| e.to_string()),
    };
    match kept {
        Ok(()) => tracing::warn!(copy = %to.display(), "run store: kept a copy of the old file"),
        Err(why) => tracing::error!(
            copy = %to.display(),
            why,
            "run store: could not keep a copy of the old file; its lines are gone"
        ),
    }
}

/// `VACUUM INTO` reads under the file's lock, so a writer of the old
/// build still attached cannot tear the copy. The target names the
/// plain-SQLite engine the way every open of this store does, so the
/// copy is one a stock `sqlite3` reads.
async fn vacuum_into(from: &Path, to: &Path) -> Result<(), sqlx::Error> {
    use sqlx::Connection;

    let bare = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(from))
        .busy_timeout(BUSY_TIMEOUT);
    let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&bare).await?;
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(datalib_runtime::plain_sqlite::uri(to))
        .execute(&mut conn)
        .await;
    conn.close().await?;
    copied.map(|_| ())
}

async fn empty_in_place(path: &Path, emptying: &Emptying) -> Result<(), sqlx::Error> {
    match emptying {
        Emptying::Reset => reset(path).await,
        Emptying::Truncate => {
            let mut journal = path.as_os_str().to_os_string();
            journal.push("-journal");
            match std::fs::remove_file(journal) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(sqlx::Error::Io(e))
                }
                _ => {}
            }
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_len(0))
                .map_err(sqlx::Error::Io)
        }
    }
}

/// A connection of its own, without [`options`]' pragmas: setting the
/// journal mode reads the header, so a file too broken to read would
/// refuse the connection that is meant to empty it.
async fn reset(path: &Path) -> Result<(), sqlx::Error> {
    use sqlx::Connection;

    let bare = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .busy_timeout(BUSY_TIMEOUT);
    let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&bare).await?;
    set_reset_database(&mut conn, true).await?;
    let vacuumed = sqlx::raw_sql("VACUUM").execute(&mut conn).await;
    conn.close().await?;
    vacuumed.map(|_| ())
}

/// Doltlite is the SQLite API; the entry point is declared here, as
/// `etl/src/doltlite_raw.rs` does, because `libsqlite3-sys` is not a
/// direct dependency.
async fn set_reset_database(
    conn: &mut sqlx::sqlite::SqliteConnection,
    on: bool,
) -> Result<(), sqlx::Error> {
    use std::ffi::{c_int, c_void};

    extern "C" {
        fn sqlite3_db_config(db: *mut c_void, op: c_int, ...) -> c_int;
    }
    const SQLITE_DBCONFIG_RESET_DATABASE: c_int = 1009;

    let mut handle = conn.lock_handle().await?;
    let db = handle.as_raw_handle().as_ptr() as *mut c_void;
    // Safety: `db` is the live connection sqlx just handed us, and this
    // op takes an int and an `int*` (which may be null).
    let rc = unsafe {
        sqlite3_db_config(
            db,
            SQLITE_DBCONFIG_RESET_DATABASE,
            c_int::from(on),
            std::ptr::null_mut::<c_int>(),
        )
    };
    if rc != 0 {
        return Err(sqlx::Error::Protocol(format!(
            "SQLITE_DBCONFIG_RESET_DATABASE {on}: sqlite rc {rc}"
        )));
    }
    Ok(())
}

/// Which build wrote this file, beside its tables. The ladder position
/// is `SCHEMA_VERSION` itself: this store already has one, and
/// `PRAGMA user_version` stays what `schema_matches` reads.
async fn write_meta(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let hash = datalib_store_meta::schema_hash(
        app_schema::runs::ddl()
            .into_iter()
            .chain(INDEXES.iter().copied()),
    );
    datalib_store_meta::write(
        pool,
        datalib_store_meta::StoreKind::Runs,
        &hash,
        datalib_store_meta::Versions {
            schema: SCHEMA_VERSION as u32,
            shared: 0,
        },
    )
    .await
    .map_err(|e| sqlx::Error::Protocol(format!("_datalib_meta: {e:#}")))?;
    Ok(())
}

/// `true` when the file carries this build's schema; `false` for another
/// version's. A brand-new file (version 0, no tables) gets the schema
/// installed and reads as a match.
async fn schema_matches(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let version: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version == SCHEMA_VERSION {
        return Ok(true);
    }
    let tables: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
        .fetch_one(pool)
        .await?;
    if version == 0 && tables == 0 {
        install_schema(pool).await?;
        return Ok(true);
    }
    Ok(false)
}

/// One transaction, because another process deciding what to do with
/// this file decides by what it can see: tables without the version
/// stamp read as a store from some other build, and
/// [`open_or_recreate`] answers one of those by deleting it. The DDL is
/// all `IF NOT EXISTS`, so two processes installing at once is the
/// second one finding it already done.
async fn install_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for ddl in app_schema::runs::ddl()
        .into_iter()
        .chain(INDEXES.iter().copied())
    {
        sqlx::query(ddl).execute(&mut *tx).await?;
    }
    // Safe: a compile-time integer, not input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {SCHEMA_VERSION}"
    )))
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// `k=v` pairs joined with `,`, in key order — one spelling per label
/// set, so it can be part of a primary key.
pub fn canonical_labels(labels: &BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// One run in the store, as a reader sees it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    /// Which run these rows describe. `None` for an empty or absent
    /// store. A reader comparing this against the run it is displaying is
    /// how it avoids painting one run's numbers onto another.
    pub run_id: Option<String>,
    pub started_at_utc: Option<String>,
    pub finished_at_utc: Option<String>,
    pub tz_offset: Option<String>,
    pub steps: Vec<StepRunRow>,
    pub metrics: Vec<MetricRow>,
    /// `warn` and `error` log rows per step — the E of USE.
    pub errors: BTreeMap<String, i64>,
    /// When each step last logged anything (UTC), for telling a step
    /// that is busy but not advancing from one that has gone silent.
    pub last_log_at: BTreeMap<String, String>,
    /// Every sample of a queue's series — `datalib_metrics::QUEUED`, and
    /// the running totals of what came off it, `DONE` and `DEQUEUED` — from the last
    /// [`QUEUE_WINDOW`], plus the newest one before it, per series: where
    /// each stood when the window opened and how it moved since. Oldest
    /// first within a series.
    pub queue_history: Vec<MetricSampleRow>,
}

pub async fn snapshot(data_root: &Path) -> Snapshot {
    snapshot_of(data_root, None).await
}

/// One run by id, or the newest when `run_id` is `None`.
pub async fn snapshot_of(data_root: &Path, run_id: Option<&str>) -> Snapshot {
    let path = runs_path(data_root);
    if !path.exists() {
        return Snapshot::default();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Snapshot::default();
    };
    let out = read_snapshot(&pool, run_id).await.unwrap_or_default();
    pool.close().await;
    out
}

/// How many times each part of the store has been written, for a
/// watcher that saw the file move and wants to know which readers to
/// wake. Empty for a missing store; a part never written is absent.
pub async fn versions(data_root: &Path) -> BTreeMap<StorePart, i64> {
    let path = runs_path(data_root);
    if !path.exists() {
        return BTreeMap::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return BTreeMap::new();
    };
    let rows = sqlx::query("SELECT what, version FROM store_changes")
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .filter_map(|r| {
            let what: String = r.get("what");
            Some((StorePart::parse(&what)?, r.get::<i64, _>("version")))
        })
        .collect()
}

/// The processes the store holds, newest first: the server's launches,
/// the runners and their steps' attempts — how a reader introspects
/// the server's own log the way it does a run's. `run` narrows to one
/// run's; `kind` to one [`Process`] word.
pub async fn processes(
    data_root: &Path,
    run: Option<&str>,
    kind: Option<&str>,
    limit: i64,
) -> Vec<ProcessRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT process_id, process, run_id, step, attempt, started_at_utc, finished_at_utc, \
           exit_code, signal, tz_offset, git_hash \
         FROM processes WHERE (? IS NULL OR run_id = ?) AND (? IS NULL OR process = ?) \
         ORDER BY started_at_utc DESC LIMIT ?",
    )
    .bind(run)
    .bind(run)
    .bind(kind)
    .bind(kind)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter().map(process_row_from).collect()
}

/// Recent runs, newest first. With `step`, only the runs that step took
/// part in — how a reader finds the run a step's log is in.
pub async fn runs(data_root: &Path, step: Option<&str>, limit: i64) -> Vec<RunRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT r.run_id, r.started_at_utc, r.finished_at_utc, r.tz_offset FROM runs r \
         WHERE ? IS NULL OR EXISTS \
           (SELECT 1 FROM step_runs s WHERE s.run_id = r.run_id AND s.step = ?) \
         ORDER BY r.started_at_utc DESC LIMIT ?",
    )
    .bind(step)
    .bind(step)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .map(|r| RunRow {
            run_id: r.get("run_id"),
            started_at_utc: r.get("started_at_utc"),
            finished_at_utc: r.get("finished_at_utc"),
            tz_offset: r.get("tz_offset"),
        })
        .collect()
}

/// The newest sample of one metric series per step, across every run
/// the store keeps: what a step last counted, and in which run. A
/// step that has never reported the series is absent — the reader
/// draws "not counted", never a false zero.
pub async fn latest_metric(data_root: &Path, name: &str) -> Vec<MetricRow> {
    newest_per_series(data_root, Some(name)).await
}

/// The newest sample of every series every step has reported, across
/// every run the store keeps: what `GET /metrics` serves.
pub async fn latest_metrics(data_root: &Path) -> Vec<MetricRow> {
    newest_per_series(data_root, None).await
}

async fn newest_per_series(data_root: &Path, name: Option<&str>) -> Vec<MetricRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    // Every sample, newest run first within a (step, name, labels)
    // series; the first of each is the answer. Two runs
    // started in the same instant — a test, or two ticks of a wall
    // clock at second resolution — fall back to the order the store
    // recorded them in.
    let rows = sqlx::query(
        "SELECT m.run_id, m.step, m.name, m.labels, m.value, m.updated_at_utc, m.tz_offset \
         FROM metrics m JOIN runs r ON r.run_id = m.run_id \
         WHERE ?1 IS NULL OR m.name = ?1 \
         ORDER BY m.step, m.name, m.labels, r.started_at_utc DESC, r.rowid DESC",
    )
    .bind(name)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    let mut out: Vec<MetricRow> = Vec::new();
    for r in &rows {
        let step: String = r.get("step");
        let name: String = r.get("name");
        let labels: String = r.get("labels");
        if out
            .last()
            .is_some_and(|m| m.step == step && m.name == name && m.labels == labels)
        {
            continue;
        }
        out.push(MetricRow {
            run_id: r.get("run_id"),
            step,
            name,
            labels,
            value: r.get("value"),
            updated_at_utc: r.get("updated_at_utc"),
            tz_offset: r.get("tz_offset"),
        });
    }
    out
}

/// One metric series per (step, labels), across every run the store
/// keeps, from `since_utc` on — plus the newest sample before it, so a
/// series that has not moved inside the window still has the value it
/// held there. Oldest first within a series; compacted, so a reader
/// carries each value forward rather than assuming an interval.
pub async fn metric_history(data_root: &Path, name: &str, since_utc: &str) -> Vec<MetricSampleRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT run_id, step, labels, ts_utc, tz_offset, value FROM ( \
           SELECT *, ROW_NUMBER() OVER (PARTITION BY step, labels ORDER BY ts_utc DESC) AS rn \
           FROM metric_samples WHERE name = ?1 AND ts_utc <= ?2) \
         WHERE rn = 1 \
         UNION ALL \
         SELECT run_id, step, labels, ts_utc, tz_offset, value \
           FROM metric_samples WHERE name = ?1 AND ts_utc > ?2 \
         ORDER BY step, labels, ts_utc",
    )
    .bind(name)
    .bind(since_utc)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .map(|r| MetricSampleRow {
            run_id: r.get("run_id"),
            step: r.get("step"),
            name: name.to_string(),
            labels: r.get("labels"),
            ts_utc: r.get("ts_utc"),
            tz_offset: r.get("tz_offset"),
            value: r.get("value"),
        })
        .collect()
}

async fn read_snapshot(pool: &SqlitePool, run_id: Option<&str>) -> Result<Snapshot, sqlx::Error> {
    let Some(run) = sqlx::query(
        "SELECT run_id, started_at_utc, finished_at_utc, tz_offset FROM runs \
         WHERE ? IS NULL OR run_id = ? ORDER BY started_at_utc DESC LIMIT 1",
    )
    .bind(run_id)
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Snapshot::default());
    };
    let run_id: String = run.get("run_id");
    let steps = sqlx::query(
        "SELECT step, state, attempt, started_at_utc, finished_at_utc, error, msg, updated_at_utc, tz_offset \
         FROM step_runs WHERE run_id = ? ORDER BY step",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| StepRunRow {
        run_id: run_id.clone(),
        step: r.get("step"),
        state: r.get("state"),
        attempt: r.get("attempt"),
        started_at_utc: r.get("started_at_utc"),
        finished_at_utc: r.get("finished_at_utc"),
        error: r.get("error"),
        msg: r.get("msg"),
        updated_at_utc: r.get("updated_at_utc"),
        tz_offset: r.get("tz_offset"),
    })
    .collect();
    let metrics = sqlx::query(
        "SELECT step, name, labels, value, updated_at_utc, tz_offset FROM metrics \
         WHERE run_id = ? ORDER BY step, name, labels",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| MetricRow {
        run_id: run_id.clone(),
        step: r.get("step"),
        name: r.get("name"),
        labels: r.get("labels"),
        value: r.get("value"),
        updated_at_utc: r.get("updated_at_utc"),
        tz_offset: r.get("tz_offset"),
    })
    .collect();
    let errors = sqlx::query(
        "SELECT step, COUNT(*) AS n FROM log \
         WHERE run_id = ? AND step IS NOT NULL AND level IN ('warn', 'error') GROUP BY step",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<i64, _>("n")))
    .collect();
    let last_log_at = sqlx::query(
        "SELECT step, MAX(ts_utc) AS ts_utc FROM log WHERE run_id = ? AND step IS NOT NULL GROUP BY step",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<String, _>("ts_utc")))
    .collect();
    let (queue_cutoff, _) = datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(QUEUE_WINDOW.as_micros() as i64))
        .to_utc_and_offset();
    // The names are bound from `datalib_metrics`, the one spelling the
    // runner writes and the ETA reads: a series this query missed would
    // not fail, it would make the estimate count a whole run as taken
    // off inside the window.
    let queue_history = sqlx::query(
        "SELECT step, name, labels, ts_utc, tz_offset, value FROM ( \
           SELECT *, ROW_NUMBER() OVER (PARTITION BY step, name, labels ORDER BY ts_utc DESC) AS rn \
           FROM metric_samples WHERE run_id = ?1 AND name IN (?3, ?4, ?5) AND ts_utc <= ?2) \
         WHERE rn = 1 \
         UNION ALL \
         SELECT step, name, labels, ts_utc, tz_offset, value FROM metric_samples \
           WHERE run_id = ?1 AND name IN (?3, ?4, ?5) AND ts_utc > ?2 \
         ORDER BY step, name, labels, ts_utc",
    )
    .bind(&run_id)
    .bind(&queue_cutoff)
    .bind(datalib_metrics::QUEUED)
    .bind(datalib_metrics::DONE)
    .bind(datalib_metrics::DEQUEUED)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| MetricSampleRow {
        run_id: run_id.clone(),
        step: r.get("step"),
        name: r.get("name"),
        labels: r.get("labels"),
        ts_utc: r.get("ts_utc"),
        tz_offset: r.get("tz_offset"),
        value: r.get("value"),
    })
    .collect();
    Ok(Snapshot {
        run_id: Some(run_id),
        started_at_utc: run.get("started_at_utc"),
        finished_at_utc: run.get("finished_at_utc"),
        tz_offset: run.get("tz_offset"),
        steps,
        metrics,
        errors,
        last_log_at,
        queue_history,
    })
}

/// Recent runs that any of `steps` took part in, newest first: the runs
/// a group's dashboard can show.
pub async fn runs_of_steps(data_root: &Path, steps: &[String], limit: i64) -> Vec<RunRow> {
    let mut out: Vec<RunRow> = Vec::new();
    for step in steps {
        for r in runs(data_root, Some(step), limit).await {
            if !out.iter().any(|x| x.run_id == r.run_id) {
                out.push(r);
            }
        }
    }
    out.sort_by(|a, b| b.started_at_utc.cmp(&a.started_at_utc));
    out.truncate(limit.max(0) as usize);
    out
}

/// Every sample of every series one step reported in one run, oldest
/// first within a series.
pub async fn step_samples(data_root: &Path, run_id: &str, step: &str) -> Vec<MetricSampleRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT name, labels, ts_utc, tz_offset, value FROM metric_samples \
         WHERE run_id = ? AND step = ? ORDER BY name, labels, ts_utc",
    )
    .bind(run_id)
    .bind(step)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .map(|r| MetricSampleRow {
            run_id: run_id.to_string(),
            step: step.to_string(),
            name: r.get("name"),
            labels: r.get("labels"),
            ts_utc: r.get("ts_utc"),
            tz_offset: r.get("tz_offset"),
            value: r.get("value"),
        })
        .collect()
}

/// When each `warn` and `error` line one step wrote in one run was
/// written, oldest first, as `(level, ts_utc)`.
pub async fn step_problem_lines(
    data_root: &Path,
    run_id: &str,
    step: &str,
) -> Vec<(String, String)> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT level, ts_utc FROM log \
         WHERE run_id = ? AND step = ? AND level IN ('warn', 'error') ORDER BY ts_utc",
    )
    .bind(run_id)
    .bind(step)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .map(|r| (r.get::<String, _>("level"), r.get::<String, _>("ts_utc")))
        .collect()
}

/// One line by its `seq`, with what its process says about it.
pub async fn log_line(data_root: &Path, seq: i64) -> Option<LogLine> {
    let path = runs_path(data_root);
    if !path.exists() {
        return None;
    }
    let pool = open_existing(&path).await.ok()?;
    // Audited: the column list is this module's own constant.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
         WHERE l.seq = ?"
    )))
    .bind(seq)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    pool.close().await;
    row.as_ref().map(log_line_from)
}

/// One process by id.
pub async fn process(data_root: &Path, process_id: &str) -> Option<ProcessRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return None;
    }
    let pool = open_existing(&path).await.ok()?;
    let row = sqlx::query(
        "SELECT process_id, process, run_id, step, attempt, started_at_utc, finished_at_utc, \
           exit_code, signal, tz_offset, git_hash \
         FROM processes WHERE process_id = ?",
    )
    .bind(process_id)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    pool.close().await;
    row.as_ref().map(process_row_from)
}

fn process_row_from(r: &sqlx::sqlite::SqliteRow) -> ProcessRow {
    ProcessRow {
        process_id: r.get("process_id"),
        process: r.get("process"),
        run_id: r.get("run_id"),
        step: r.get("step"),
        attempt: r.get("attempt"),
        started_at_utc: r.get("started_at_utc"),
        finished_at_utc: r.get("finished_at_utc"),
        exit_code: r.get("exit_code"),
        signal: r.get("signal"),
        tz_offset: r.get("tz_offset"),
        git_hash: r.get("git_hash"),
    }
}

/// Log lines of one run after `after_seq`, oldest first, at most `limit`.
/// `step` narrows to one step; `None` is the whole run. This is the
/// tail: a client keeps the last `seq` it saw and asks again.
pub async fn log_after(
    data_root: &Path,
    run_id: &str,
    step: Option<&str>,
    after_seq: i64,
    limit: i64,
) -> Vec<LogLine> {
    log_where(data_root, Some(run_id), step, after_seq, limit).await
}

async fn log_where(
    data_root: &Path,
    run_id: Option<&str>,
    step: Option<&str>,
    after_seq: i64,
    limit: i64,
) -> Vec<LogLine> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    // Audited: the column list is this module's own constant.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
         WHERE (? IS NULL OR l.run_id = ?) AND l.seq > ? AND (? IS NULL OR l.step = ?) \
         ORDER BY l.seq LIMIT ?"
    )))
    .bind(run_id)
    .bind(run_id)
    .bind(after_seq)
    .bind(step)
    .bind(step)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter().map(log_line_from).collect()
}

/// Lines written outside any run after `after_seq`, oldest first, at
/// most `limit`: what the server's watcher reads to learn which of its
/// own requests moved the log.
pub async fn process_log_after(data_root: &Path, after_seq: i64, limit: i64) -> Vec<LogRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    // Audited: the column list is this module's own constant.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
         WHERE l.run_id IS NULL AND l.seq > ? ORDER BY l.seq LIMIT ?"
    )))
    .bind(after_seq)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter().map(|r| log_line_from(r).row).collect()
}

/// The newest line's `seq`, or 0 for an empty or absent store.
pub async fn last_log_seq(data_root: &Path) -> i64 {
    let path = runs_path(data_root);
    if !path.exists() {
        return 0;
    }
    let Ok(pool) = open_existing(&path).await else {
        return 0;
    };
    let seq: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM log")
        .fetch_one(&pool)
        .await
        .ok()
        .flatten();
    pool.close().await;
    seq.unwrap_or(0)
}

/// A log line as a reader sees it: the row, with what its process says
/// about it — which program wrote it and from which commit. Both
/// `None` for a line whose process the store no longer has.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    #[serde(flatten)]
    pub row: LogRow,
    /// A [`Process`] word.
    pub process: Option<String>,
    pub git_hash: Option<String>,
}

impl std::ops::Deref for LogLine {
    type Target = LogRow;
    fn deref(&self) -> &LogRow {
        &self.row
    }
}

/// The columns [`log_line_from`] reads, for a query that selects them
/// itself: `log` as `l`, joined to `processes` as `p`.
pub(crate) const LOG_LINE_COLUMNS: &str = "l.seq, l.run_id, l.process_id, l.step, l.group_id, \
     l.attempt, l.ts_utc, l.tz_offset, l.stream, l.level, l.target, l.thread, l.msg, l.fields, \
     p.process, p.git_hash";

pub(crate) fn log_line_from(r: &sqlx::sqlite::SqliteRow) -> LogLine {
    LogLine {
        row: LogRow {
            seq: r.get("seq"),
            run_id: r.get("run_id"),
            process_id: r.get("process_id"),
            step: r.get("step"),
            group_id: r.get("group_id"),
            attempt: r.get("attempt"),
            ts_utc: r.get("ts_utc"),
            tz_offset: r.get("tz_offset"),
            stream: r.get("stream"),
            level: r.get("level"),
            target: r.get("target"),
            thread: r.get("thread"),
            msg: r.get("msg"),
            fields: r.get("fields"),
        },
        process: r.get("process"),
        git_hash: r.get("git_hash"),
    }
}

/// What names one metric series: its step, its name and its labels.
type SeriesKey = (String, String, String);

/// What has been published and not yet written. Step and metric updates
/// coalesce to the newest; log lines never do.
#[derive(Default)]
struct Pending {
    steps: BTreeMap<String, StepRunRow>,
    /// The runner's steps, each attempt its own process: started, then
    /// the same row again with its end. Newest wins.
    processes: BTreeMap<String, ProcessRow>,
    logs: Vec<LogRow>,
    metrics: BTreeMap<SeriesKey, MetricRow>,
}

type Shared = Arc<Mutex<Pending>>;

/// Something that takes log lines. Both writers below are one, which is
/// what lets the tracing layer feed either.
pub trait LogSink: Send + Sync {
    fn log(&self, row: LogRow);
}

/// Sent to the writer thread to have it flush now, and answered once
/// that flush is over.
type FlushAck = tokio::sync::oneshot::Sender<()>;

/// The thread behind a writer, and the handles that stop it. Dropping
/// the sender tells the thread to flush once more and exit, which is
/// what makes the final rows land. It must be dropped *before* joining
/// — joining while still holding the sender waits forever on a thread
/// that has not been told to stop.
struct Writer {
    process_id: String,
    pending: Shared,
    flushes: Mutex<Option<mpsc::Sender<FlushAck>>>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Writer {
    /// Waits for the thread to open the store, so a store that will not
    /// open is `None` here — where the caller says nothing will be
    /// recorded — rather than a writer that takes every line and keeps
    /// none of them.
    fn start(data_root: &Path, scope: Scope) -> Option<Self> {
        let path = runs_path(data_root);
        let process_id = scope.process.process_id.clone();
        let pending: Shared = Default::default();
        let (tx, rx) = mpsc::channel();
        let (opened_tx, opened) = mpsc::sync_channel(1);
        let handle = std::thread::Builder::new()
            .name("run-store".into())
            .spawn({
                let pending = pending.clone();
                let path = path.clone();
                move || writer_loop(path, scope, pending, rx, opened_tx)
            })
            .ok()?;
        let failed = match opened.recv() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("the writer thread ended before opening it".to_string()),
        };
        if let Some(error) = failed {
            let _ = handle.join();
            tracing::error!(
                path = %path.display(),
                error,
                "run store: could not open it; nothing will be recorded"
            );
            return None;
        }
        Some(Self {
            process_id,
            pending,
            flushes: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        })
    }

    fn log(&self, row: LogRow) {
        self.pending.lock().expect("run store mutex").logs.push(row);
    }

    async fn flush(&self) {
        let (ack, flushed) = tokio::sync::oneshot::channel();
        let sent = self
            .flushes
            .lock()
            .expect("run store flush mutex")
            .as_ref()
            .is_some_and(|tx| tx.send(ack).is_ok());
        // A thread that has gone drops the ack, which ends the wait as
        // surely as an answer.
        if sent {
            let _ = flushed.await;
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Sender first — that disconnect is the loop's exit signal.
        drop(self.flushes.lock().expect("run store flush mutex").take());
        let handle = self.handle.lock().expect("run store handle mutex").take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// Publishes one run to the store. Cheap to call; the work happens on
/// its own thread. Every row it takes belongs to the run it was started
/// for and was written by the runner, so a row's own `run_id` and
/// `process_id` are ignored and those are bound instead.
pub struct RunWriter(Writer);

impl RunWriter {
    /// `git_hash` is the commit the runner came from ([`crate::git_hash`]),
    /// or `None` when a dev build cannot say.
    pub fn start(
        data_root: &Path,
        run_id: &str,
        started_at_utc: &str,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Option<Self> {
        // The start stamp arrives as the runner wrote it, offset and
        // all; the store keeps UTC and the offset apart. The runner's
        // process starts when its run does.
        let (started_at_utc, tz_offset) = split_stamp(started_at_utc);
        let process = ProcessRow {
            process_id: mint_process_id(),
            process: Process::Dag.as_str().into(),
            run_id: Some(run_id.to_string()),
            started_at_utc: started_at_utc.clone(),
            tz_offset: tz_offset.clone(),
            git_hash,
            ..Default::default()
        };
        let run = RunInfo {
            run_id: run_id.to_string(),
            started_at_utc,
            tz_offset,
        };
        Writer::start(
            data_root,
            Scope {
                process,
                run: Some(run),
                retention,
            },
        )
        .map(Self)
    }

    pub fn step(&self, next: StepRunRow) {
        let mut p = self.0.pending.lock().expect("run store mutex");
        // A terminal state latches: a tick that was already in flight
        // when the step finished must not resurrect it as running.
        if let Some(prev) = p.steps.get(&next.step) {
            if is_terminal(&prev.state) && !is_terminal(&next.state) {
                return;
            }
        }
        p.steps.insert(next.step.clone(), next);
    }

    pub fn log(&self, row: LogRow) {
        self.0.log(row);
    }

    pub fn metric(&self, row: MetricRow) {
        let key = (row.step.clone(), row.name.clone(), row.labels.clone());
        self.0
            .pending
            .lock()
            .expect("run store mutex")
            .metrics
            .insert(key, row);
    }

    /// A process of this run other than the runner — a step attempt —
    /// as it starts, and again as it ends with `finished_at_utc` and
    /// the exit filled in. `run_id` is bound to this run whatever the
    /// row says.
    pub fn process(&self, row: ProcessRow) {
        self.0
            .pending
            .lock()
            .expect("run store mutex")
            .processes
            .insert(row.process_id.clone(), row);
    }

    /// Returns once everything published before the call is committed,
    /// or lost with a warning.
    pub async fn flush(&self) {
        self.0.flush().await;
    }

    /// The runner's own process id, for a line it writes about itself.
    pub fn process_id(&self) -> &str {
        &self.0.process_id
    }
}

/// A fresh process id, for a runner naming a step it is about to spawn.
pub fn new_process_id() -> String {
    mint_process_id()
}

impl LogSink for RunWriter {
    fn log(&self, row: LogRow) {
        self.0.log(row);
    }
}

/// Publishes the log of a process that is not a run — the app's
/// server — to the same table, with no `run_id`. The rows it takes are
/// bound to the process the way a [`RunWriter`]'s are to its run;
/// `git_hash` is the commit the process came from ([`crate::git_hash`]).
pub struct ProcessLogWriter(Writer);

impl ProcessLogWriter {
    pub fn start(
        data_root: &Path,
        process: Process,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Option<Self> {
        let (started_at_utc, tz_offset) = now_split();
        let process = ProcessRow {
            process_id: mint_process_id(),
            process: process.as_str().into(),
            started_at_utc,
            tz_offset,
            git_hash,
            ..Default::default()
        };
        Writer::start(
            data_root,
            Scope {
                process,
                run: None,
                retention,
            },
        )
        .map(Self)
    }

    pub fn log(&self, row: LogRow) {
        self.0.log(row);
    }

    /// Returns once everything logged before the call is committed, or
    /// lost with a warning.
    pub async fn flush(&self) {
        self.0.flush().await;
    }

    /// A process this one records on behalf of — a page of the app,
    /// which cannot reach the store itself — as it starts, and again
    /// with `finished_at_utc` set when it says it is going.
    pub fn process(&self, row: ProcessRow) {
        self.0
            .pending
            .lock()
            .expect("run store mutex")
            .processes
            .insert(row.process_id.clone(), row);
    }

    /// The row this launch writes under, for the server to say which
    /// of the store's launches it is.
    pub fn process_id(&self) -> &str {
        &self.0.process_id
    }
}

impl LogSink for ProcessLogWriter {
    fn log(&self, row: LogRow) {
        self.0.log(row);
    }
}

struct RunInfo {
    run_id: String,
    started_at_utc: String,
    tz_offset: Option<String>,
}

/// What a writer thread is writing on behalf of: always a process, and
/// a run of it when the process is the runner.
struct Scope {
    process: ProcessRow,
    run: Option<RunInfo>,
    retention: Retention,
}

impl Scope {
    fn run_id(&self) -> Option<&str> {
        self.run.as_ref().map(|r| r.run_id.as_str())
    }
}

fn mint_process_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// What the writer thread remembers per metric series, to decide when
/// a sample is due.
struct SeriesState {
    last_sample_at: Instant,
    last_sample_value: i64,
    current: MetricRow,
}

fn writer_loop(
    path: PathBuf,
    scope: Scope,
    pending: Shared,
    flushes: mpsc::Receiver<FlushAck>,
    opened: mpsc::SyncSender<Result<(), String>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = opened.send(Err(format!("build its runtime: {e}")));
            return;
        }
    };
    let pool = match rt.block_on(open_or_recreate(&path)) {
        Ok(pool) => pool,
        Err(e) => {
            let _ = opened.send(Err(e.to_string()));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    if let Err(e) = rt.block_on(begin(&pool, &scope)) {
        tracing::warn!(error = %e, "run store: could not record the start");
    }

    let mut series: HashMap<SeriesKey, SeriesState> = HashMap::new();
    let mut last_prune = Instant::now();
    loop {
        // Wake on the interval, on a flush asked for, or when the writer
        // is dropped — whose disconnect is the signal to flush and go.
        let (ack, done) = match flushes.recv_timeout(FLUSH_EVERY) {
            Ok(ack) => (Some(ack), false),
            Err(RecvTimeoutError::Timeout) => (None, false),
            Err(RecvTimeoutError::Disconnected) => (None, true),
        };
        let batch = {
            let mut p = pending.lock().expect("run store mutex");
            std::mem::take(&mut *p)
        };
        let lines = batch.logs.len();
        if let Err(e) = rt.block_on(flush_or_retry(&pool, &scope, &batch, &mut series, done)) {
            // One bad flush must not stop the record for every other
            // step, and must never take the run down. Say how many
            // lines went with it: the count is the only trace left of
            // them, and a silent loss here is what makes anyone
            // distrust the store.
            tracing::warn!(error = %e, lines, "run store: write failed twice; the batch is lost");
        }
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
        if done {
            break;
        }
        if scope.run.is_none() && last_prune.elapsed() >= PRUNE_EVERY {
            if let Err(e) = rt.block_on(prune_unowned(&pool, scope.retention)) {
                tracing::warn!(error = %e, "run store: could not prune old lines");
            }
            last_prune = Instant::now();
        }
    }
    if let Err(e) = rt.block_on(end(&pool, &scope)) {
        tracing::warn!(error = %e, "run store: could not close the record");
    }
    rt.block_on(pool.close());
}

/// An ISO stamp with an offset, as UTC plus that offset. A stamp that
/// will not parse is kept as written, with no offset — better a line
/// with an odd clock than a line lost.
pub fn split_stamp(iso: &str) -> (String, Option<String>) {
    match datalib_time::parse_strict(iso) {
        Ok(t) => {
            let (utc, offset) = t.to_utc_and_offset();
            (utc, Some(offset))
        }
        Err(_) => (iso.to_string(), None),
    }
}

pub fn now_split() -> (String, Option<String>) {
    let (utc, offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    (utc, Some(offset))
}

/// Record the process, and its run when it has one, and apply
/// retention. Their own rows go in first so the count limit includes
/// them.
async fn begin(pool: &SqlitePool, scope: &Scope) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    insert_process(&mut tx, &scope.process).await?;
    if let Some(run) = &scope.run {
        begin_run(&mut tx, run, scope.retention).await?;
    }
    tx.commit().await?;
    prune_unowned(pool, scope.retention).await
}

/// Idempotent, and called again with every batch of lines: a process
/// that outlives the age cutoff and has no lines left in the store
/// is pruned, and must not then write lines that name no process.
async fn insert_process(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    p: &ProcessRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO processes \
           (process_id, process, run_id, step, attempt, started_at_utc, finished_at_utc, \
            exit_code, signal, tz_offset, git_hash) \
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?)",
    )
    .bind(&p.process_id)
    .bind(&p.process)
    .bind(&p.run_id)
    .bind(&p.step)
    .bind(p.attempt)
    .bind(&p.started_at_utc)
    .bind(&p.tz_offset)
    .bind(&p.git_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A step's process, as the runner reports it: the whole row at start,
/// and the same row with its end and exit once it is over.
async fn upsert_process(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    p: &ProcessRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO processes \
           (process_id, process, run_id, step, attempt, started_at_utc, finished_at_utc, \
            exit_code, signal, tz_offset, git_hash) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(process_id) DO UPDATE SET finished_at_utc = excluded.finished_at_utc, \
           exit_code = excluded.exit_code, signal = excluded.signal",
    )
    .bind(&p.process_id)
    .bind(&p.process)
    .bind(&p.run_id)
    .bind(&p.step)
    .bind(p.attempt)
    .bind(&p.started_at_utc)
    .bind(&p.finished_at_utc)
    .bind(p.exit_code)
    .bind(p.signal)
    .bind(&p.tz_offset)
    .bind(&p.git_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn begin_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run: &RunInfo,
    retention: Retention,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runs (run_id, started_at_utc, tz_offset) VALUES (?, ?, ?) \
         ON CONFLICT(run_id) DO UPDATE SET started_at_utc = excluded.started_at_utc, \
           tz_offset = excluded.tz_offset, finished_at_utc = NULL",
    )
    .bind(&run.run_id)
    .bind(&run.started_at_utc)
    .bind(&run.tz_offset)
    .execute(&mut **tx)
    .await?;
    let cutoff = age_cutoff(retention, &run.started_at_utc);
    sqlx::query("DELETE FROM runs WHERE started_at_utc < ? AND run_id != ?")
        .bind(&cutoff)
        .bind(&run.run_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "DELETE FROM runs WHERE run_id NOT IN \
         (SELECT run_id FROM runs ORDER BY started_at_utc DESC LIMIT ?)",
    )
    .bind(retention.max_runs.max(1) as i64)
    .execute(&mut **tx)
    .await?;
    for table in ["processes", "step_runs", "log", "metrics", "metric_samples"] {
        // Safe: the five names are the literals above, never input.
        // `NOT IN` is false for a NULL `run_id`, so the server's
        // processes and lines are kept here and aged out by
        // `prune_unowned`.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE run_id NOT IN (SELECT run_id FROM runs)"
        )))
        .execute(&mut **tx)
        .await?;
    }
    bump(tx, StorePart::Runs).await
}

/// What [`close_abandoned_run`] found to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClosedRun {
    /// Whether the run itself was still open. False when the runner
    /// closed its own books, which is the ordinary case.
    pub run_was_open: bool,
    /// Steps that were still `pending` or `running`.
    pub steps_closed: u64,
}

impl ClosedRun {
    pub fn changed_anything(self) -> bool {
        self.run_was_open || self.steps_closed > 0
    }
}

/// Close a run whose runner is gone, so nothing reads `running` for ever.
///
/// A runner normally closes its own books. One that was SIGKILLed ran no
/// code to do it with, and the row it leaves behind is what the Manage
/// screen joins against — so whoever outlives the runner has to finish
/// the sentence. Idempotent: a run already closed is left alone and
/// reported as such.
///
/// `step_state` is the caller's word for what a step that never reported
/// became, because the scheduler's vocabulary is not this crate's
/// business (see [`crate::LiveState`]). `why` goes on each step's
/// `error`, so a person can tell a step that stopped itself from one the
/// server gave up on. A step still `pending` takes it too, where [`end`]
/// deletes one: a runner that died never settled the steps it was asked
/// for, so a pending row here may be one it was about to run.
///
/// Processes are deliberately untouched. This crate would have to invent
/// an exit code or a signal for them, and it does not know one: the
/// runner records how a step ended when it gets to wait for it, and when
/// it does not, "we never found out" is the honest answer.
pub async fn close_abandoned_run(
    data_root: &Path,
    run_id: &str,
    step_state: &str,
    why: &str,
) -> Result<ClosedRun, sqlx::Error> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Ok(ClosedRun::default());
    }
    let pool = open_existing(&path).await?;
    let out = close_abandoned_run_in(&pool, run_id, step_state, why).await;
    pool.close().await;
    out
}

async fn close_abandoned_run_in(
    pool: &SqlitePool,
    run_id: &str,
    step_state: &str,
    why: &str,
) -> Result<ClosedRun, sqlx::Error> {
    let (at, tz_offset) = now_split();
    let mut tx = pool.begin().await?;

    let run_was_open = sqlx::query(
        "UPDATE runs SET finished_at_utc = ?, tz_offset = coalesce(tz_offset, ?) \
         WHERE run_id = ? AND finished_at_utc IS NULL",
    )
    .bind(&at)
    .bind(&tz_offset)
    .bind(run_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;

    // The non-terminal states, from the one place that names them, so a
    // new `LiveState` variant is covered without editing this.
    let live: Vec<&'static str> = <LiveState as strum::VariantArray>::VARIANTS
        .iter()
        .map(|s| s.as_str())
        .collect();
    let holes = std::iter::repeat_n("?", live.len())
        .collect::<Vec<_>>()
        .join(", ");
    // Audited: the only interpolation is `holes`, which is placeholders
    // built from a count; every value below is bound.
    let sql = format!(
        "UPDATE step_runs SET state = ?, finished_at_utc = coalesce(finished_at_utc, ?), \
           error = coalesce(error, ?), updated_at_utc = ?, tz_offset = coalesce(tz_offset, ?) \
         WHERE run_id = ? AND state IN ({holes})"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(step_state)
        .bind(&at)
        .bind(why)
        .bind(&at)
        .bind(&tz_offset)
        .bind(run_id);
    for state in &live {
        q = q.bind(*state);
    }
    let steps_closed = q.execute(&mut *tx).await?.rows_affected();

    if run_was_open {
        bump(&mut tx, StorePart::Runs).await?;
    }
    if steps_closed > 0 {
        bump(&mut tx, StorePart::StepRuns).await?;
    }
    tx.commit().await?;
    Ok(ClosedRun {
        run_was_open,
        steps_closed,
    })
}

/// Count one write to `what`, inside the transaction that made it.
/// Retention is deliberately not counted: it takes lines away that no
/// tail is waiting on, and a watcher woken for it would find nothing
/// new to show.
async fn bump(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    what: StorePart,
) -> Result<(), sqlx::Error> {
    let (changed_at_utc, tz_offset) = now_split();
    sqlx::query(
        "INSERT INTO store_changes (what, version, changed_at_utc, tz_offset) VALUES (?, 1, ?, ?) \
         ON CONFLICT(what) DO UPDATE SET version = store_changes.version + 1, \
           changed_at_utc = excluded.changed_at_utc, tz_offset = excluded.tz_offset",
    )
    .bind(what.as_str())
    .bind(changed_at_utc)
    .bind(tz_offset)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Retention for what belongs to no run: the lines have their own,
/// shorter age and a row cap — a server at `debug` between syncs must
/// not grow the file for a month — and a process goes once it is past
/// that age with no run and no line left to name it.
async fn prune_unowned(pool: &SqlitePool, retention: Retention) -> Result<(), sqlx::Error> {
    let cutoff = cutoff_days_ago(retention.process_log_days);
    sqlx::query("DELETE FROM log WHERE run_id IS NULL AND ts_utc < ?")
        .bind(&cutoff)
        .execute(pool)
        .await?;
    sqlx::query(
        "DELETE FROM log WHERE run_id IS NULL AND seq < \
         (SELECT seq FROM log WHERE run_id IS NULL ORDER BY seq DESC LIMIT 1 OFFSET ?)",
    )
    .bind(retention.process_log_lines.max(1) as i64 - 1)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM processes WHERE run_id IS NULL AND started_at_utc < ? \
         AND process_id NOT IN (SELECT process_id FROM log)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;
    Ok(())
}

/// The UTC stamp before which a run is older than retention keeps.
/// Text order is instant order, so this is one comparison.
///
/// Measured from the run being begun, not from the wall clock: a run
/// is stamped with the runner's now, which `--now` can pin, and a root
/// run under a pinned clock would otherwise sweep its whole history on
/// the next run. When the runner is not pinned the two clocks agree.
/// The wall clock remains only for a start stamp that is not one.
fn age_cutoff(retention: Retention, newest_started_at_utc: &str) -> String {
    let newest = datalib_time::parse_strict(newest_started_at_utc)
        .unwrap_or_else(|_| datalib_time::IsoOffsetTimestamp::now_local());
    days_before(newest, retention.max_age_days)
}

fn cutoff_days_ago(days: u32) -> String {
    days_before(datalib_time::IsoOffsetTimestamp::now_local(), days)
}

fn days_before(at: datalib_time::IsoOffsetTimestamp, days: u32) -> String {
    at.bump_micros(-(days as i64) * 86_400 * 1_000_000)
        .to_utc_and_offset()
        .0
}

/// The process is over, and its run with it. Counted as a change to
/// the runs so a watcher redraws a run that just finished; a launch
/// ending is nobody's live question. By now the runner has settled
/// every step a request reached, so a step still `pending` is one no
/// request asked for: it was not part of this run, and its row goes
/// rather than reading as waiting forever.
async fn end(pool: &SqlitePool, scope: &Scope) -> Result<(), sqlx::Error> {
    let (finished_at_utc, _) = now_split();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE processes SET finished_at_utc = ? WHERE process_id = ?")
        .bind(&finished_at_utc)
        .bind(&scope.process.process_id)
        .execute(&mut *tx)
        .await?;
    if let Some(run) = &scope.run {
        sqlx::query("UPDATE runs SET finished_at_utc = ? WHERE run_id = ?")
            .bind(&finished_at_utc)
            .bind(&run.run_id)
            .execute(&mut *tx)
            .await?;
        bump(&mut tx, StorePart::Runs).await?;
        let never_started = sqlx::query("DELETE FROM step_runs WHERE run_id = ? AND state = ?")
            .bind(&run.run_id)
            .bind(LiveState::Pending.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if never_started > 0 {
            bump(&mut tx, StorePart::StepRuns).await?;
        }
    }
    tx.commit().await
}

/// One retry, because a flush is one transaction: a failure wrote
/// nothing, so the same batch can simply be handed over again. The
/// reason to expect a second attempt to work is the reason for the
/// first failure — the other process held the write lock past
/// [`BUSY_TIMEOUT`] — which is over by the time it starts.
async fn flush_or_retry(
    pool: &SqlitePool,
    scope: &Scope,
    batch: &Pending,
    series: &mut HashMap<SeriesKey, SeriesState>,
    last: bool,
) -> Result<(), sqlx::Error> {
    let Err(first) = flush(pool, scope, batch, series, last).await else {
        return Ok(());
    };
    tracing::debug!(error = %first, "run store: write failed; retrying once");
    tokio::time::sleep(RETRY_AFTER).await;
    flush(pool, scope, batch, series, last).await
}

/// Writes the batch in one transaction, and moves `series` on only if
/// that transaction commits — so a retry sees the sample state its
/// failed predecessor found, not the one it would have installed.
async fn flush(
    pool: &SqlitePool,
    scope: &Scope,
    batch: &Pending,
    series: &mut HashMap<SeriesKey, SeriesState>,
    last: bool,
) -> Result<(), sqlx::Error> {
    let empty = batch.steps.is_empty()
        && batch.processes.is_empty()
        && batch.logs.is_empty()
        && batch.metrics.is_empty();
    let now = Instant::now();
    let plan = plan_series(series, &batch.metrics, now, last);
    if empty && !last && plan.samples.is_empty() {
        return Ok(());
    }
    let run_id = scope.run_id();
    let own_process_id = scope.process.process_id.as_str();
    let mut tx = pool.begin().await?;
    for p in batch.processes.values() {
        upsert_process(
            &mut tx,
            &ProcessRow {
                run_id: run_id.map(str::to_string),
                ..p.clone()
            },
        )
        .await?;
    }
    if !batch.steps.is_empty() {
        bump(&mut tx, StorePart::StepRuns).await?;
    }
    if !batch.logs.is_empty() {
        insert_process(&mut tx, &scope.process).await?;
        let part = match scope.run {
            Some(_) => StorePart::RunLog,
            None => StorePart::ProcessLog,
        };
        bump(&mut tx, part).await?;
    }
    if !batch.metrics.is_empty() {
        bump(&mut tx, StorePart::Metrics).await?;
    }
    for s in batch.steps.values() {
        sqlx::query(
            "INSERT INTO step_runs \
               (run_id, step, state, attempt, started_at_utc, finished_at_utc, error, msg, updated_at_utc, \
                tz_offset) \
             VALUES (?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(run_id, step) DO UPDATE SET \
               state=excluded.state, attempt=excluded.attempt, \
               started_at_utc=COALESCE(excluded.started_at_utc, step_runs.started_at_utc), \
               finished_at_utc=excluded.finished_at_utc, error=excluded.error, \
               msg=excluded.msg, updated_at_utc=excluded.updated_at_utc, tz_offset=excluded.tz_offset",
        )
        .bind(run_id)
        .bind(&s.step)
        .bind(&s.state)
        .bind(s.attempt)
        .bind(&s.started_at_utc)
        .bind(&s.finished_at_utc)
        .bind(&s.error)
        .bind(&s.msg)
        .bind(&s.updated_at_utc)
        .bind(&s.tz_offset)
        .execute(&mut *tx)
        .await?;
    }
    for l in &batch.logs {
        let process_id = if l.process_id.is_empty() {
            own_process_id
        } else {
            l.process_id.as_str()
        };
        sqlx::query(
            "INSERT INTO log (run_id, process_id, step, group_id, attempt, ts_utc, tz_offset, \
                              stream, level, target, thread, msg, fields) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(run_id)
        .bind(process_id)
        .bind(&l.step)
        .bind(&l.group_id)
        .bind(l.attempt)
        .bind(&l.ts_utc)
        .bind(&l.tz_offset)
        .bind(&l.stream)
        .bind(&l.level)
        .bind(&l.target)
        .bind(&l.thread)
        .bind(&l.msg)
        .bind(&l.fields)
        .execute(&mut *tx)
        .await?;
    }
    for m in batch.metrics.values() {
        sqlx::query(
            "INSERT INTO metrics (run_id, step, name, labels, value, updated_at_utc, tz_offset) \
             VALUES (?,?,?,?,?,?,?) \
             ON CONFLICT(run_id, step, name, labels) DO UPDATE SET \
               value=excluded.value, updated_at_utc=excluded.updated_at_utc, tz_offset=excluded.tz_offset",
        )
        .bind(run_id)
        .bind(&m.step)
        .bind(&m.name)
        .bind(&m.labels)
        .bind(m.value)
        .bind(&m.updated_at_utc)
        .bind(&m.tz_offset)
        .execute(&mut *tx)
        .await?;
    }
    for m in &plan.samples {
        insert_sample(&mut tx, run_id, m).await?;
    }
    tx.commit().await?;
    for (key, state) in plan.next {
        series.insert(key, state);
    }
    Ok(())
}

/// What a flush is about to do to the sample series: the rows to write,
/// and the state to install once its transaction commits.
struct SeriesPlan {
    samples: Vec<MetricRow>,
    next: Vec<(SeriesKey, SeriesState)>,
}

/// A sample is due when a series has moved and the floor between
/// samples has passed. A move the floor held back is written once the
/// floor has passed, even if the series has not moved again — at the
/// stamp it moved at: a queue that fills and then empties in one pass
/// would otherwise keep its peak in the history until its next move. On
/// the last flush of a run, whatever each series ended at is written
/// however recent its previous sample: a rate drawn to the end of the
/// run needs that point.
fn plan_series(
    series: &HashMap<SeriesKey, SeriesState>,
    batch: &BTreeMap<SeriesKey, MetricRow>,
    now: Instant,
    last: bool,
) -> SeriesPlan {
    let mut plan = SeriesPlan {
        samples: Vec::new(),
        next: Vec::new(),
    };
    for (key, m) in batch {
        let previous = series.get(key);
        let due = match previous {
            None => true,
            Some(s) => s.last_sample_value != m.value && now - s.last_sample_at >= SAMPLE_EVERY,
        };
        // `previous` is Some wherever this is reached: a series with no
        // state behind it is always due.
        let closing = last && !due && previous.is_some_and(|s| s.last_sample_value != m.value);
        if due || closing {
            plan.samples.push(m.clone());
        }
        plan.next.push((
            key.clone(),
            SeriesState {
                last_sample_at: match (due, previous) {
                    (false, Some(s)) => s.last_sample_at,
                    _ => now,
                },
                last_sample_value: match (due || closing, previous) {
                    (false, Some(s)) => s.last_sample_value,
                    _ => m.value,
                },
                current: m.clone(),
            },
        ));
    }
    for (key, s) in series {
        let held_back = !batch.contains_key(key) && s.current.value != s.last_sample_value;
        if !held_back || !(last || now - s.last_sample_at >= SAMPLE_EVERY) {
            continue;
        }
        plan.samples.push(s.current.clone());
        plan.next.push((
            key.clone(),
            SeriesState {
                last_sample_at: now,
                last_sample_value: s.current.value,
                current: s.current.clone(),
            },
        ));
    }
    plan
}

/// Steps and metrics belong to a run, so `run_id` is bound as given:
/// a process-scoped writer never queues either, and if one ever did the
/// table's NOT NULL would refuse the row rather than file it under a
/// run that does not exist.
async fn insert_sample(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: Option<&str>,
    m: &MetricRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO metric_samples (run_id, step, name, labels, ts_utc, tz_offset, value) \
         VALUES (?,?,?,?,?,?,?)",
    )
    .bind(run_id)
    .bind(&m.step)
    .bind(&m.name)
    .bind(&m.labels)
    .bind(&m.updated_at_utc)
    .bind(&m.tz_offset)
    .bind(m.value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue that fills and then empties within the sample floor: the
    /// drop is held back, then written once the floor has passed — with
    /// nothing new in the batch, and at the stamp it happened — rather
    /// than leaving the peak in the history until the queue next moves.
    #[test]
    fn a_move_the_floor_held_back_is_written_once_it_passes() {
        let key: SeriesKey = ("s".into(), "queued".into(), String::new());
        let row = |value: i64, at: &str| MetricRow {
            step: "s".into(),
            name: "queued".into(),
            value,
            updated_at_utc: at.into(),
            ..Default::default()
        };
        let t0 = Instant::now();
        let mut series = HashMap::new();
        let apply = |series: &mut HashMap<SeriesKey, SeriesState>, plan: SeriesPlan| {
            for (k, st) in plan.next {
                series.insert(k, st);
            }
            plan.samples
                .iter()
                .map(|m| (m.value, m.updated_at_utc.clone()))
                .collect::<Vec<_>>()
        };
        let batch = |m: MetricRow| BTreeMap::from([(key.clone(), m)]);

        let first = plan_series(&series, &batch(row(50, "t1")), t0, false);
        assert_eq!(apply(&mut series, first), [(50, "t1".to_string())]);
        let inside = plan_series(
            &series,
            &batch(row(0, "t2")),
            t0 + Duration::from_secs(1),
            false,
        );
        assert_eq!(apply(&mut series, inside), [], "inside the floor");
        let quiet = plan_series(
            &series,
            &BTreeMap::new(),
            t0 + Duration::from_secs(3),
            false,
        );
        assert_eq!(apply(&mut series, quiet), [], "still inside the floor");
        let after = plan_series(&series, &BTreeMap::new(), t0 + SAMPLE_EVERY, false);
        assert_eq!(apply(&mut series, after), [(0, "t2".to_string())]);
        let again = plan_series(&series, &BTreeMap::new(), t0 + SAMPLE_EVERY * 3, false);
        assert_eq!(apply(&mut series, again), [], "written once");
    }

    /// A store from another schema version, with a reader that opened
    /// it before a writer remade it — `datalib-http`'s reads take no
    /// lock. When the remake unlinked the file, the reader stayed on
    /// the old inode, took the new file's journal for a crashed
    /// writer's, rolled it back and deleted it, and the writer's commit
    /// failed with 5898 (`SQLITE_IOERR_DELETE_NOENT`). CI run
    /// 36441513702 lost a whole writer's lines this way.
    #[tokio::test]
    async fn a_reader_open_across_the_remake_leaves_the_writers_journal_alone() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("runs.sqlite");
        let planted = open_or_create(&path).await.unwrap();
        sqlx::raw_sql("CREATE TABLE ancient (x TEXT); PRAGMA user_version = 1")
            .execute(&planted)
            .await
            .unwrap();
        planted.close().await;

        let reader = open_existing(&path).await.unwrap();
        let tables = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'";
        sqlx::query_scalar::<_, i64>(tables)
            .fetch_one(&reader)
            .await
            .unwrap();

        let writer = open_or_recreate(&path).await.unwrap();
        let mut tx = writer.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO log (process_id, attempt, ts_utc, level, msg) \
             VALUES ('p', 0, 't', 'info', 'kept')",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query_scalar::<_, i64>(tables)
            .fetch_one(&reader)
            .await
            .unwrap();
        tx.commit()
            .await
            .expect("the writer's commit, after a reader read mid-transaction");

        let kept: Vec<String> = sqlx::query_scalar("SELECT msg FROM log")
            .fetch_all(&reader)
            .await
            .unwrap();
        assert_eq!(kept, ["kept"], "the reader sees the remade store");
        reader.close().await;
        writer.close().await;
    }

    /// The bytes of a file that is not a database are kept as they
    /// were, beside the file emptied for the new store.
    #[tokio::test]
    async fn a_file_that_is_not_a_database_is_emptied_and_opens() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("runs.sqlite");
        let junk = b"this is not a database, sqlite or otherwise";
        std::fs::write(&path, junk).unwrap();
        let pool = open_or_recreate(&path).await.expect("open the store");
        assert!(schema_matches(&pool).await.unwrap());
        pool.close().await;
        let kept: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("runs.bak_"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read(td.path().join(&kept[0])).unwrap(), junk);
    }

    #[test]
    fn the_copy_is_named_for_the_moment_in_utc() {
        let at = datalib_time::parse_strict("2026-09-30T10:15:02-04:00").unwrap();
        assert_eq!(
            backup_path(Path::new("/r/system/runs/runs.sqlite"), &at),
            Path::new("/r/system/runs/runs.bak_20260930T141502Z.sqlite")
        );
    }
}
