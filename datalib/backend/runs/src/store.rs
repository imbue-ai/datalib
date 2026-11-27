//! Writing the store, and reading it.

use datalib_time::RunClock;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use datalib_flock::FileLock;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::redact::Redactor;
use crate::{
    is_terminal, runs_path, LiveState, Retention, INDEXES, INVOCATIONS_BY_START,
    INVOCATIONS_BY_STEP, INVOCATION_METRICS_BY_NAME, INVOCATION_SAMPLES_BY_NAME, LOG_BY_INVOCATION,
    METRICS_BY_NAME, METRIC_SAMPLES_BY_NAME, PROCESSES_BY_INVOCATION, PROCESSES_BY_RUN,
    REQUESTS_BY_REQUEST, RUNS_BY_START, SCHEMA_VERSION,
};
use anyhow::Context as _;
use app_schema::runs::{
    InvocationMetricRow, InvocationRequestRow, InvocationRow, LogRow, Process, ProcessRow, RunRow,
    StorePart,
};
use datalib_sql::{sql, sql_id, Sql};

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

/// How often a writer re-applies retention. Every writer prunes as it
/// starts; the loop and the server each run for weeks, and have to do it
/// again as they go.
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
        // WAL, so a reader never waits on a writer. In a rollback
        // journal every read queued behind every commit, which a busy
        // disk made seconds long (docs/dev/logging.md § One store). A
        // file in the old mode turns over at its first open.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // Nothing here is load-bearing, so nothing is worth an fsync. A
        // file torn by a power cut is emptied and remade on the next
        // open (see `open_or_recreate`).
        .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
        .pragma(
            "journal_size_limit",
            datalib_runtime::plain_sqlite::WAL_SIZE_LIMIT_BYTES,
        )
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

/// The store, open for reading in this build's shape; `None` before any
/// process has made it. A store in another shape is refused: the next
/// writer's open brings it to this one ([`open_or_recreate`]).
pub(crate) async fn open_reader(data_root: &Path) -> anyhow::Result<Option<SqlitePool>> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Ok(None);
    }
    let pool = open_existing(&path).await?;
    let checked = match datalib_store_meta::read(&pool).await {
        Ok(meta) => datalib_store_meta::require(&path, None, meta.as_ref(), &shape()),
        Err(e) => Err(e),
    };
    if let Err(e) = checked {
        pool.close().await;
        return Err(e);
    }
    Ok(Some(pool))
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

/// Make the store under `data_root` in this build's shape, and close it:
/// for a process that will read it while another writes, so neither
/// meets a file half made.
pub async fn prepare_store(data_root: &Path) -> Result<(), sqlx::Error> {
    open_or_recreate(&runs_path(data_root)).await?.close().await;
    Ok(())
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
    /// Cut the file to nothing, and its journal and WAL with it.
    /// Doltlite reads a file without SQLite's header as its own format
    /// and refuses the reset, and it refuses every other connection's
    /// use of the file the same way, so none can be reading or writing
    /// it.
    Truncate,
}

fn emptying_for(head: &[u8]) -> Emptying {
    if head.starts_with(b"SQLite format 3\0") {
        Emptying::Reset
    } else {
        Emptying::Truncate
    }
}

/// Opens a second descriptor of the file and closes it, which drops every
/// POSIX lock this process holds on the file, SQLite's included
/// (fcntl(2); `datalib_dag::supervisor::announce` was bitten by it). Safe
/// only because [`open_or_recreate`] calls it with no connection to the
/// file open in this process: its own pools are closed, every reader
/// opens and closes its own, and each process's one writer is the caller.
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
    let copied = sql!("runs.vacuum_into_copy", "VACUUM INTO ?")
        .query()
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
            for sidecar in ["-journal", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_os_string();
                file.push(sidecar);
                match std::fs::remove_file(file) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(sqlx::Error::Io(e))
                    }
                    _ => {}
                }
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
    let vacuumed = sql!("runs.vacuum_reset", "VACUUM")
        .raw()
        .execute(&mut conn)
        .await;
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

/// The shape this build writes the store in.
fn shape() -> datalib_store_meta::Shape {
    datalib_store_meta::Shape {
        tables: datalib_store_meta::schema_hash(
            app_schema::runs::ddl()
                .into_iter()
                .chain(INDEXES.iter().copied()),
        ),
        indexes: None,
        versions: datalib_store_meta::Versions {
            schema: SCHEMA_VERSION as u32,
            shared: 0,
        },
    }
}

/// Which build wrote this file, beside its tables. The ladder position
/// is `SCHEMA_VERSION` itself: this store already has one, and
/// `PRAGMA user_version` stays what `schema_matches` reads.
async fn write_meta(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    datalib_store_meta::write(pool, datalib_store_meta::StoreKind::Runs, &shape())
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("_datalib_meta: {e:#}")))?;
    Ok(())
}

/// The rungs from an older version: the statements that bring a store at
/// the version named to the next one, in place. Every rung is one
/// transaction, under the open lock [`open_or_recreate`] holds.
const UPGRADES: &[(i32, &[&str])] = &[
    (
        10,
        &[
            "ALTER TABLE processes ADD COLUMN applet VARCHAR(255)",
            "ALTER TABLE processes ADD COLUMN peak_rss_bytes BIGINT",
        ],
    ),
    (
        11,
        &[
            "ALTER TABLE runs ADD COLUMN kind VARCHAR(16)",
            "ALTER TABLE runs ADD COLUMN reason TEXT",
        ],
    ),
    (12, &[PROCESSES_BY_RUN]),
    (13, &[RUNS_BY_START]),
    (14, &[METRICS_BY_NAME, METRIC_SAMPLES_BY_NAME]),
    // Invocations replace runs. The run tables keep what they hold and get
    // no new rows; `log` and `processes` gain the column an invocation's
    // rows are found by, last, where a new store's DDL puts it too.
    (
        15,
        &[
            "ALTER TABLE log ADD COLUMN invocation_id VARCHAR(64)",
            "ALTER TABLE processes ADD COLUMN invocation_id VARCHAR(64)",
            app_schema::runs::invocations::DDL[0].1,
            app_schema::runs::invocation_requests::DDL[0].1,
            app_schema::runs::invocation_metrics::DDL[0].1,
            app_schema::runs::invocation_metric_samples::DDL[0].1,
            LOG_BY_INVOCATION,
            PROCESSES_BY_INVOCATION,
            INVOCATIONS_BY_START,
            INVOCATIONS_BY_STEP,
            REQUESTS_BY_REQUEST,
            INVOCATION_METRICS_BY_NAME,
            INVOCATION_SAMPLES_BY_NAME,
        ],
    ),
];

/// `true` when the file carries this build's schema, running the
/// [`UPGRADES`] up to it first where there are rungs; `false` for a version
/// with none. A brand-new file (version 0, no tables) gets the schema
/// installed and reads as a match.
async fn schema_matches(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut version: i32 = sql!("runs.read_user_version", "PRAGMA user_version")
        .query_scalar()
        .fetch_one(pool)
        .await?;
    while let Some((_, statements)) = UPGRADES.iter().find(|(from, _)| *from == version) {
        climb(pool, statements, version + 1).await?;
        version += 1;
    }
    if version == SCHEMA_VERSION {
        return Ok(true);
    }
    let tables: i32 = sql!(
        "runs.count_tables",
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'"
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;
    if version == 0 && tables == 0 {
        install_schema(pool).await?;
        return Ok(true);
    }
    Ok(false)
}

async fn climb(pool: &SqlitePool, statements: &[&str], to: i32) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for statement in statements {
        // Safe: every statement is a compile-time literal in `UPGRADES`.
        Sql::dynamic(sql_id!("runs.migrate_run_upgrade"), *statement)
            .query()
            .execute(&mut *tx)
            .await?;
    }
    // Safe: a compile-time integer, not input.
    Sql::dynamic(
        sql_id!("runs.migrate_set_user_version"),
        format!("PRAGMA user_version = {to}"),
    )
    .query()
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// One transaction, because another process deciding what to do with
/// this file decides by what it can see: tables without the version
/// stamp read as a store from some other build, and
/// [`open_or_recreate`] empties one of those, keeping a copy. The DDL is
/// all `IF NOT EXISTS`, so two processes installing at once is the
/// second one finding it already done.
async fn install_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for ddl in app_schema::runs::ddl()
        .into_iter()
        .chain(INDEXES.iter().copied())
    {
        Sql::dynamic(sql_id!("runs.create_schema"), ddl)
            .query()
            .execute(&mut *tx)
            .await?;
    }
    // Safe: a compile-time integer, not input.
    Sql::dynamic(
        sql_id!("runs.install_set_user_version"),
        format!("PRAGMA user_version = {SCHEMA_VERSION}"),
    )
    .query()
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

/// Where a metric's value or sample was reported: an invocation, or a run
/// an older build recorded. Readers of every invocation and every old run
/// get both kinds, and say which each value came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ReportedIn {
    Invocation(String),
    Run(String),
}

impl Default for ReportedIn {
    fn default() -> Self {
        ReportedIn::Invocation(String::new())
    }
}

impl ReportedIn {
    /// From a row with an `invocation_id` and a `run_id` column, one of
    /// them set. An empty invocation id is a value the runner keeps for a
    /// step it is not running.
    fn of(r: &sqlx::sqlite::SqliteRow) -> ReportedIn {
        match r.get::<Option<String>, _>("run_id") {
            Some(run) => ReportedIn::Run(run),
            None => ReportedIn::Invocation(
                r.get::<Option<String>, _>("invocation_id")
                    .unwrap_or_default(),
            ),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            ReportedIn::Invocation(id) | ReportedIn::Run(id) => id,
        }
    }
}

impl std::fmt::Display for ReportedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReportedIn::Invocation(id) if id.is_empty() => f.write_str("the runner's own count"),
            ReportedIn::Invocation(id) => write!(f, "invocation {id}"),
            ReportedIn::Run(id) => write!(f, "run {id}"),
        }
    }
}

/// One series' value, as a reader of both kinds of history gets it.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct MetricValue {
    pub reported_in: ReportedIn,
    pub step: String,
    pub name: String,
    pub labels: String,
    pub value: i64,
    pub updated_at_utc: String,
    pub tz_offset: Option<String>,
}

fn metric_value_from(r: &sqlx::sqlite::SqliteRow) -> MetricValue {
    MetricValue {
        reported_in: ReportedIn::of(r),
        step: r.get("step"),
        name: r.get("name"),
        labels: r.get("labels"),
        value: r.get("value"),
        updated_at_utc: r.get("updated_at_utc"),
        tz_offset: r.get("tz_offset"),
    }
}

/// One sample of one series, as a reader of both kinds of history gets it.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct MetricSample {
    pub reported_in: ReportedIn,
    pub step: String,
    pub name: String,
    pub labels: String,
    pub ts_utc: String,
    pub tz_offset: Option<String>,
    pub value: i64,
}

fn metric_sample_from(r: &sqlx::sqlite::SqliteRow) -> MetricSample {
    MetricSample {
        reported_in: ReportedIn::of(r),
        step: r.get("step"),
        name: r.get("name"),
        labels: r.get("labels"),
        ts_utc: r.get("ts_utc"),
        tz_offset: r.get("tz_offset"),
        value: r.get("value"),
    }
}

/// One step as a snapshot shows it: its newest invocation among those the
/// snapshot covers, or its row in an older build's run.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct SnapshotStep {
    pub step: String,
    /// `None` for a step of an older build's run.
    pub invocation_id: Option<String>,
    /// `pending` or `running` while live, else how it ended
    /// ([`crate::LiveState`]).
    pub state: String,
    pub attempt: i64,
    pub started_at_utc: Option<String>,
    pub finished_at_utc: Option<String>,
    pub error: Option<String>,
    pub msg: Option<String>,
    pub updated_at_utc: String,
    pub tz_offset: Option<String>,
}

/// What a reader draws from the store: each step's newest invocation now
/// ([`snapshot`]), or the invocations of one request or job, or one run of
/// an older build ([`snapshot_of`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    /// The request, job or old run these rows describe; `None` for the
    /// newest invocations, and for an id the store has nothing of.
    pub run_id: Option<String>,
    /// Whether `run_id` names a run an older build recorded.
    pub old_run: bool,
    /// The earliest start among the rows, and the latest end once none
    /// is running; an old run's own.
    pub started_at_utc: Option<String>,
    pub finished_at_utc: Option<String>,
    pub tz_offset: Option<String>,
    /// An old run's kind and reason; a request's are in
    /// `system/supervisor.sqlite`.
    pub kind: Option<String>,
    pub reason: Option<String>,
    pub steps: Vec<SnapshotStep>,
    pub metrics: Vec<MetricValue>,
    /// `warn` and `error` log rows per step — the E of USE.
    pub errors: BTreeMap<String, i64>,
    /// When each step last logged anything (UTC), for telling a step
    /// that is busy but not advancing from one that has gone silent.
    pub last_log_at: BTreeMap<String, String>,
    /// Every sample of a queue's series — `datalib_metrics::QUEUED`, and
    /// the running totals of what came off it, `DONE` and `DEQUEUED` — from
    /// the last [`QUEUE_WINDOW`], plus the newest one before it, per
    /// series, for the snapshot's steps: where each stood when the window
    /// opened and how it moved since. Oldest first within a series.
    pub queue_history: Vec<MetricSample>,
}

/// Each step's newest invocation, with its numbers: what the Manage screen
/// draws while it runs and after.
pub async fn snapshot(data_root: &Path) -> anyhow::Result<Snapshot> {
    snapshot_of(data_root, None).await
}

/// The invocations of one request or job by its id, or one run an older
/// build recorded; each step's newest invocation when `id` is `None`.
pub async fn snapshot_of(data_root: &Path, id: Option<&str>) -> anyhow::Result<Snapshot> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Snapshot::default());
    };
    let out = read_snapshot(&pool, id).await;
    pool.close().await;
    Ok(out?)
}

/// How many times each part of the store has been written, for a
/// watcher that saw the file move and wants to know which readers to
/// wake. Empty for a missing store; a part never written is absent.
pub async fn versions(data_root: &Path) -> anyhow::Result<BTreeMap<StorePart, i64>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(BTreeMap::new());
    };
    let rows = sql!(
        "runs.list_store_versions",
        "SELECT what, version FROM store_changes"
    )
    .query()
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?
        .iter()
        .filter_map(|r| {
            let what: String = r.get("what");
            Some((StorePart::parse(&what)?, r.get::<i64, _>("version")))
        })
        .collect())
}

/// The processes the store holds, newest first: the server's launches,
/// the runners, and each attempt of each invocation — how a reader
/// introspects the server's own log the way it does a step's. `run`
/// narrows to the attempts of one request's or job's invocations, or to
/// an older build's run; `kind` to one [`Process`] word.
pub async fn processes(
    data_root: &Path,
    run: Option<&str>,
    kind: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<ProcessRow>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.list_processes",
        "SELECT process_id, process, run_id, invocation_id, step, attempt, started_at_utc, \
           finished_at_utc, exit_code, signal, tz_offset, git_hash, applet, peak_rss_bytes \
         FROM processes WHERE (?1 IS NULL OR run_id = ?1 OR invocation_id IN \
           (SELECT invocation_id FROM invocation_requests INDEXED BY invocation_requests_by_request \
            WHERE request_id = ?1)) \
         AND (?2 IS NULL OR process = ?2) \
         ORDER BY started_at_utc DESC LIMIT ?3"
    )
    .query()
    .bind(run)
    .bind(kind)
    .bind(limit)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = rows?;
    Ok(rows.iter().map(process_row_from).collect())
}

/// The runs an older build recorded, newest first. With `step`, only the
/// runs that step took part in.
pub async fn runs(data_root: &Path, step: Option<&str>, limit: i64) -> anyhow::Result<Vec<RunRow>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.list_runs",
        "SELECT r.run_id, r.started_at_utc, r.finished_at_utc, r.tz_offset, r.kind, r.reason \
         FROM runs r \
         WHERE ? IS NULL OR EXISTS \
           (SELECT 1 FROM step_runs s WHERE s.run_id = r.run_id AND s.step = ?) \
         ORDER BY r.started_at_utc DESC LIMIT ?"
    )
    .query()
    .bind(step)
    .bind(step)
    .bind(limit)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = rows?;
    Ok(rows
        .iter()
        .map(|r| RunRow {
            run_id: r.get("run_id"),
            started_at_utc: r.get("started_at_utc"),
            finished_at_utc: r.get("finished_at_utc"),
            tz_offset: r.get("tz_offset"),
            kind: r.get("kind"),
            reason: r.get("reason"),
        })
        .collect())
}

/// The runs of an older build any of `steps` took part in, newest first.
pub async fn runs_of_steps(
    data_root: &Path,
    steps: &[String],
    limit: i64,
) -> anyhow::Result<Vec<RunRow>> {
    let mut out: Vec<RunRow> = Vec::new();
    for step in steps {
        for r in runs(data_root, Some(step), limit).await? {
            if !out.iter().any(|x| x.run_id == r.run_id) {
                out.push(r);
            }
        }
    }
    out.sort_by(|a, b| b.started_at_utc.cmp(&a.started_at_utc));
    out.truncate(limit.max(0) as usize);
    Ok(out)
}

/// The requests and jobs an invocation of any of `steps` served (every
/// one when `steps` is `None`), newest first by their newest invocation's
/// start: the ids a reader looks up in `system/supervisor.sqlite`. Each
/// with that start, UTC.
pub async fn requests_of_steps(
    data_root: &Path,
    steps: Option<&[String]>,
    limit: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let steps = steps
        .map(serde_json::to_string)
        .transpose()
        .context("the steps, as JSON")?;
    let rows = sql!(
        "runs.list_requests_of_steps",
        "SELECT q.request_id, MAX(i.started_at_utc) AS newest \
         FROM invocation_requests q JOIN invocations i USING (invocation_id) \
         WHERE ?1 IS NULL OR i.step IN (SELECT value FROM json_each(?1)) \
         GROUP BY q.request_id ORDER BY newest DESC, q.request_id DESC LIMIT ?2"
    )
    .query()
    .bind(steps)
    .bind(limit)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?
        .iter()
        .map(|r| (r.get("request_id"), r.get("newest")))
        .collect())
}

/// The invocations started for one request or job, oldest first.
pub async fn invocations_of(data_root: &Path, request: &str) -> anyhow::Result<Vec<InvocationRow>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    // Audited: the column list is this module's own constant.
    let rows = Sql::dynamic(
        sql_id!("runs.list_invocations_of_request"),
        format!(
            "SELECT {INVOCATION_COLUMNS} FROM invocations i \
             WHERE i.invocation_id IN (SELECT invocation_id FROM invocation_requests \
               INDEXED BY invocation_requests_by_request WHERE request_id = ?) \
             ORDER BY i.started_at_utc, i.invocation_id"
        ),
    )
    .query()
    .bind(request)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?.iter().map(invocation_row_from).collect())
}

const INVOCATION_COLUMNS: &str = "i.invocation_id, i.step, i.group_id, i.verb, i.state, \
     i.attempt, i.started_at_utc, i.finished_at_utc, i.now, i.fingerprint, i.reads, i.error, \
     i.msg, i.exit_code, i.signal, i.updated_at_utc, i.tz_offset";

fn invocation_row_from(r: &sqlx::sqlite::SqliteRow) -> InvocationRow {
    InvocationRow {
        invocation_id: r.get("invocation_id"),
        step: r.get("step"),
        group_id: r.get("group_id"),
        verb: r.get("verb"),
        state: r.get("state"),
        attempt: r.get("attempt"),
        started_at_utc: r.get("started_at_utc"),
        finished_at_utc: r.get("finished_at_utc"),
        now: r.get("now"),
        fingerprint: r.get("fingerprint"),
        reads: r.get("reads"),
        error: r.get("error"),
        msg: r.get("msg"),
        exit_code: r.get("exit_code"),
        signal: r.get("signal"),
        updated_at_utc: r.get("updated_at_utc"),
        tz_offset: r.get("tz_offset"),
    }
}

/// The newest value of one metric series per step, across every
/// invocation and every older build's run the store keeps: what a step
/// last counted, and where. An invocation's value is newer than any old
/// run's. A step that has never reported the series is absent — the
/// reader draws "not counted", never a false zero.
pub async fn latest_metric(data_root: &Path, name: &str) -> anyhow::Result<Vec<MetricValue>> {
    newest_per_series(data_root, Some(name)).await
}

/// One metric's value in each invocation and each older build's run the
/// store keeps, per (step, labels): for a running total, what each came
/// to. Oldest first within a series.
pub async fn metric_by_invocation(
    data_root: &Path,
    name: &str,
) -> anyhow::Result<Vec<MetricValue>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.list_metric_by_invocation",
        "SELECT m.invocation_id, NULL AS run_id, m.step, m.name, m.labels, m.value, \
           m.updated_at_utc, m.tz_offset, 1 AS newer, i.started_at_utc AS began, \
           m.invocation_id AS tiebreak \
         FROM invocation_metrics m INDEXED BY invocation_metrics_by_name \
           JOIN invocations i USING (invocation_id) WHERE m.name = ?1 \
         UNION ALL \
         SELECT NULL, m.run_id, m.step, m.name, m.labels, m.value, m.updated_at_utc, \
           m.tz_offset, 0, r.started_at_utc, r.rowid \
         FROM metrics m INDEXED BY metrics_by_name JOIN runs r ON r.run_id = m.run_id \
         WHERE m.name = ?1 \
         ORDER BY step, labels, newer, began, tiebreak"
    )
    .query()
    .bind(name)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?.iter().map(metric_value_from).collect())
}

/// The newest value of every series every step has reported, across the
/// whole store: what `GET /metrics` serves.
pub async fn latest_metrics(data_root: &Path) -> anyhow::Result<Vec<MetricValue>> {
    newest_per_series(data_root, None).await
}

async fn newest_per_series(
    data_root: &Path,
    name: Option<&str>,
) -> anyhow::Result<Vec<MetricValue>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    // Every value, newest first within a (step, name, labels) series; the
    // first of each is the answer. Invocations before old runs, then by
    // start; two started in the same instant fall back to the invocation's
    // id (a UUID v7, minted in start order) or the order the old store
    // recorded its runs in. A value the runner keeps for a step between
    // invocations has no invocation row, and is dated by its own stamp.
    let rows = match name {
        Some(name) => sql!(
            "runs.newest_metric_per_series_by_name",
            "SELECT m.invocation_id, NULL AS run_id, m.step, m.name, m.labels, m.value, \
               m.updated_at_utc, m.tz_offset, 1 AS newer, \
               COALESCE(i.started_at_utc, m.updated_at_utc) AS began, m.invocation_id AS tiebreak \
             FROM invocation_metrics m INDEXED BY invocation_metrics_by_name \
               LEFT JOIN invocations i USING (invocation_id) WHERE m.name = ?1 \
             UNION ALL \
             SELECT NULL, m.run_id, m.step, m.name, m.labels, m.value, m.updated_at_utc, \
               m.tz_offset, 0, r.started_at_utc, r.rowid \
             FROM metrics m INDEXED BY metrics_by_name JOIN runs r ON r.run_id = m.run_id \
             WHERE m.name = ?1 \
             ORDER BY step, name, labels, newer DESC, began DESC, tiebreak DESC"
        )
        .query()
        .bind(name),
        None => sql!(
            "runs.newest_metric_per_series",
            "SELECT m.invocation_id, NULL AS run_id, m.step, m.name, m.labels, m.value, \
               m.updated_at_utc, m.tz_offset, 1 AS newer, \
               COALESCE(i.started_at_utc, m.updated_at_utc) AS began, m.invocation_id AS tiebreak \
             FROM invocation_metrics m LEFT JOIN invocations i USING (invocation_id) \
             UNION ALL \
             SELECT NULL, m.run_id, m.step, m.name, m.labels, m.value, m.updated_at_utc, \
               m.tz_offset, 0, r.started_at_utc, r.rowid \
             FROM metrics m JOIN runs r ON r.run_id = m.run_id \
             ORDER BY step, name, labels, newer DESC, began DESC, tiebreak DESC"
        )
        .query(),
    }
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = rows?;
    let mut out: Vec<MetricValue> = Vec::new();
    for r in &rows {
        let m = metric_value_from(r);
        if out
            .last()
            .is_some_and(|l| l.step == m.step && l.name == m.name && l.labels == m.labels)
        {
            continue;
        }
        out.push(m);
    }
    Ok(out)
}

/// One metric series per (step, labels), across every invocation and old
/// run the store keeps, from `since_utc` on — plus the newest sample
/// before it, so a series that has not moved inside the window still has
/// the value it held there. Oldest first within a series; compacted, so a
/// reader carries each value forward rather than assuming an interval.
pub async fn metric_history(
    data_root: &Path,
    name: &str,
    since_utc: &str,
) -> anyhow::Result<Vec<MetricSample>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.metric_history_since",
        "WITH s AS ( \
           SELECT invocation_id, NULL AS run_id, step, name, labels, ts_utc, tz_offset, value \
             FROM invocation_metric_samples INDEXED BY invocation_samples_by_name WHERE name = ?1 \
           UNION ALL \
           SELECT NULL, run_id, step, name, labels, ts_utc, tz_offset, value \
             FROM metric_samples INDEXED BY metric_samples_by_name WHERE name = ?1) \
         SELECT invocation_id, run_id, step, name, labels, ts_utc, tz_offset, value FROM ( \
           SELECT *, ROW_NUMBER() OVER (PARTITION BY step, labels ORDER BY ts_utc DESC) AS rn \
           FROM s WHERE ts_utc <= ?2) \
         WHERE rn = 1 \
         UNION ALL \
         SELECT invocation_id, run_id, step, name, labels, ts_utc, tz_offset, value \
           FROM s WHERE ts_utc > ?2 \
         ORDER BY step, labels, ts_utc"
    )
    .query()
    .bind(name)
    .bind(since_utc)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?.iter().map(metric_sample_from).collect())
}

/// The invocations a snapshot covers: each step's newest among all of
/// them, or among one request's or job's.
async fn newest_per_step(
    pool: &SqlitePool,
    request: Option<&str>,
) -> Result<Vec<String>, sqlx::Error> {
    match request {
        None => {
            sql!(
                "runs.list_newest_invocation_per_step",
                "SELECT invocation_id FROM ( \
               SELECT invocation_id, ROW_NUMBER() OVER \
                 (PARTITION BY step ORDER BY started_at_utc DESC, invocation_id DESC) AS rn \
               FROM invocations INDEXED BY invocations_by_step) \
             WHERE rn = 1"
            )
            .query_scalar()
            .fetch_all(pool)
            .await
        }
        Some(request) => {
            sql!(
                "runs.list_newest_invocation_per_step_of_request",
                "SELECT invocation_id FROM ( \
               SELECT i.invocation_id, ROW_NUMBER() OVER \
                 (PARTITION BY i.step ORDER BY i.started_at_utc DESC, i.invocation_id DESC) AS rn \
               FROM invocation_requests q INDEXED BY invocation_requests_by_request \
                 JOIN invocations i USING (invocation_id) \
               WHERE q.request_id = ?) \
             WHERE rn = 1"
            )
            .query_scalar()
            .bind(request)
            .fetch_all(pool)
            .await
        }
    }
}

async fn read_snapshot(pool: &SqlitePool, id: Option<&str>) -> Result<Snapshot, sqlx::Error> {
    if let Some(id) = id {
        if let Some(old) = read_old_run(pool, id).await? {
            return Ok(old);
        }
    }
    let ids = newest_per_step(pool, id).await?;
    if id.is_some() && ids.is_empty() {
        return Ok(Snapshot::default());
    }
    let ids_json = serde_json::to_string(&ids).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    // Audited: the column list is this module's own constant.
    let rows: Vec<InvocationRow> = Sql::dynamic(
        sql_id!("runs.list_snapshot_invocations"),
        format!(
            "SELECT {INVOCATION_COLUMNS} FROM invocations i \
             WHERE i.invocation_id IN (SELECT value FROM json_each(?)) ORDER BY i.step"
        ),
    )
    .query()
    .bind(&ids_json)
    .fetch_all(pool)
    .await?
    .iter()
    .map(invocation_row_from)
    .collect();
    let steps: Vec<SnapshotStep> = rows
        .iter()
        .map(|i| SnapshotStep {
            step: i.step.clone(),
            invocation_id: Some(i.invocation_id.clone()),
            state: i.state.clone(),
            attempt: i.attempt,
            started_at_utc: Some(i.started_at_utc.clone()),
            finished_at_utc: i.finished_at_utc.clone(),
            error: i.error.clone(),
            msg: i.msg.clone(),
            updated_at_utc: i.updated_at_utc.clone(),
            tz_offset: i.tz_offset.clone(),
        })
        .collect();
    // A value the runner keeps for a step it is not running is the live
    // one only when nothing newer stands for it.
    let metrics = sql!(
        "runs.list_metrics_of_invocations",
        "SELECT invocation_id, NULL AS run_id, step, name, labels, value, updated_at_utc, \
           tz_offset FROM invocation_metrics \
         WHERE invocation_id IN (SELECT value FROM json_each(?1)) \
            OR (?2 AND invocation_id = '') \
         ORDER BY step, name, labels, updated_at_utc"
    )
    .query()
    .bind(&ids_json)
    .bind(id.is_none())
    .fetch_all(pool)
    .await?
    .iter()
    .map(metric_value_from)
    .collect();
    let errors = sql!(
        "runs.count_problem_lines_per_invocation",
        "SELECT step, COUNT(*) AS n FROM log INDEXED BY log_by_invocation \
         WHERE invocation_id IN (SELECT value FROM json_each(?)) \
           AND step IS NOT NULL AND level IN ('warn', 'error') GROUP BY step"
    )
    .query()
    .bind(&ids_json)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<i64, _>("n")))
    .collect();
    let last_log_at = sql!(
        "runs.latest_log_per_invocation",
        "SELECT step, MAX(ts_utc) AS ts_utc FROM log INDEXED BY log_by_invocation \
         WHERE invocation_id IN (SELECT value FROM json_each(?)) AND step IS NOT NULL \
         GROUP BY step"
    )
    .query()
    .bind(&ids_json)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<String, _>("ts_utc")))
    .collect();
    let steps_json = serde_json::to_string(&steps.iter().map(|s| &s.step).collect::<Vec<_>>())
        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let (queue_cutoff, _) = datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(QUEUE_WINDOW.as_micros() as i64))
        .to_utc_and_offset();
    // The names are bound from `datalib_metrics`, the one spelling the
    // runner writes and the ETA reads: a series this query missed would
    // not fail, it would make the estimate count a whole run as taken
    // off inside the window. By step, whichever invocation took them: a
    // consumer's queue moves between its invocations too.
    let queue_history = sql!(
        "runs.latest_progress_samples",
        "WITH s AS ( \
           SELECT invocation_id, NULL AS run_id, step, name, labels, ts_utc, tz_offset, value \
           FROM invocation_metric_samples INDEXED BY invocation_samples_by_name \
           WHERE name IN (?3, ?4, ?5) AND step IN (SELECT value FROM json_each(?1))) \
         SELECT invocation_id, run_id, step, name, labels, ts_utc, tz_offset, value FROM ( \
           SELECT *, ROW_NUMBER() OVER (PARTITION BY step, name, labels ORDER BY ts_utc DESC) AS rn \
           FROM s WHERE ts_utc <= ?2) \
         WHERE rn = 1 \
         UNION ALL \
         SELECT invocation_id, run_id, step, name, labels, ts_utc, tz_offset, value \
           FROM s WHERE ts_utc > ?2 \
         ORDER BY step, name, labels, ts_utc"
    )
    .query()
    .bind(&steps_json)
    .bind(&queue_cutoff)
    .bind(datalib_metrics::QUEUED)
    .bind(datalib_metrics::DONE)
    .bind(datalib_metrics::DEQUEUED)
    .fetch_all(pool)
    .await?
    .iter()
    .map(metric_sample_from)
    .collect();
    let started_at_utc = rows.iter().map(|i| i.started_at_utc.clone()).min();
    let finished_at_utc = if rows.iter().all(|i| i.finished_at_utc.is_some()) {
        rows.iter().filter_map(|i| i.finished_at_utc.clone()).max()
    } else {
        None
    };
    Ok(Snapshot {
        run_id: id.map(str::to_string),
        old_run: false,
        started_at_utc,
        finished_at_utc,
        tz_offset: rows.first().and_then(|i| i.tz_offset.clone()),
        kind: None,
        reason: None,
        steps,
        metrics,
        errors,
        last_log_at,
        queue_history,
    })
}

/// One run an older build recorded, as it recorded it; `None` when `id`
/// names none.
async fn read_old_run(pool: &SqlitePool, run_id: &str) -> Result<Option<Snapshot>, sqlx::Error> {
    let Some(run) = sql!(
        "runs.read_run",
        "SELECT run_id, started_at_utc, finished_at_utc, tz_offset, kind, reason \
         FROM runs WHERE run_id = ?"
    )
    .query()
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let steps = sql!(
        "runs.list_step_runs_of_run",
        "SELECT step, state, attempt, started_at_utc, finished_at_utc, error, msg, \
           updated_at_utc, tz_offset FROM step_runs WHERE run_id = ? ORDER BY step"
    )
    .query()
    .bind(run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| SnapshotStep {
        step: r.get("step"),
        invocation_id: None,
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
    let metrics = sql!(
        "runs.list_metrics_of_run",
        "SELECT NULL AS invocation_id, run_id, step, name, labels, value, updated_at_utc, \
           tz_offset FROM metrics WHERE run_id = ? ORDER BY step, name, labels"
    )
    .query()
    .bind(run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(metric_value_from)
    .collect();
    let errors = sql!(
        "runs.count_problem_lines_per_step",
        "SELECT step, COUNT(*) AS n FROM log \
         WHERE run_id = ? AND step IS NOT NULL AND level IN ('warn', 'error') GROUP BY step"
    )
    .query()
    .bind(run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<i64, _>("n")))
    .collect();
    let last_log_at = sql!(
        "runs.latest_log_per_step",
        "SELECT step, MAX(ts_utc) AS ts_utc FROM log WHERE run_id = ? AND step IS NOT NULL \
         GROUP BY step"
    )
    .query()
    .bind(run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("step"), r.get::<String, _>("ts_utc")))
    .collect();
    Ok(Some(Snapshot {
        run_id: Some(run_id.to_string()),
        old_run: true,
        started_at_utc: run.get("started_at_utc"),
        finished_at_utc: run.get("finished_at_utc"),
        tz_offset: run.get("tz_offset"),
        kind: run.get("kind"),
        reason: run.get("reason"),
        steps,
        metrics,
        errors,
        last_log_at,
        // An old run's queue is over: nothing to estimate from it.
        queue_history: Vec::new(),
    }))
}

/// Every sample of every series one step reported for one request or job
/// (all its invocations of the step), or in one older build's run; oldest
/// first within a series.
pub async fn step_samples(
    data_root: &Path,
    run: &str,
    step: &str,
) -> anyhow::Result<Vec<MetricSample>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.list_step_samples",
        "SELECT invocation_id, NULL AS run_id, step, name, labels, ts_utc, tz_offset, value \
           FROM invocation_metric_samples \
           WHERE step = ?2 AND invocation_id IN (SELECT invocation_id FROM invocation_requests \
             INDEXED BY invocation_requests_by_request WHERE request_id = ?1) \
         UNION ALL \
         SELECT NULL, run_id, step, name, labels, ts_utc, tz_offset, value FROM metric_samples \
           WHERE run_id = ?1 AND step = ?2 \
         ORDER BY name, labels, ts_utc"
    )
    .query()
    .bind(run)
    .bind(step)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    Ok(rows?.iter().map(metric_sample_from).collect())
}

/// When each `warn` and `error` line one step wrote for one request or
/// job, or in one older build's run, was written, oldest first, as
/// `(level, ts_utc)`.
pub async fn step_problem_lines(
    data_root: &Path,
    run: &str,
    step: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    let rows = sql!(
        "runs.list_step_problem_lines",
        "SELECT level, ts_utc FROM log INDEXED BY log_by_invocation \
           WHERE step = ?2 AND level IN ('warn', 'error') AND invocation_id IN \
             (SELECT invocation_id FROM invocation_requests \
              INDEXED BY invocation_requests_by_request WHERE request_id = ?1) \
         UNION ALL \
         SELECT level, ts_utc FROM log INDEXED BY log_by_run_step \
           WHERE run_id = ?1 AND step = ?2 AND level IN ('warn', 'error') \
         ORDER BY ts_utc"
    )
    .query()
    .bind(run)
    .bind(step)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = rows?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("level"), r.get::<String, _>("ts_utc")))
        .collect())
}

/// One line by its `seq`, with what its process says about it.
pub async fn log_line(data_root: &Path, seq: i64) -> anyhow::Result<Option<LogLine>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(None);
    };
    // Audited: the column list is this module's own constant.
    let row = Sql::dynamic(
        sql_id!("runs.read_log_line"),
        format!(
            "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
         WHERE l.seq = ?"
        ),
    )
    .query()
    .bind(seq)
    .fetch_optional(&pool)
    .await;
    pool.close().await;
    let row = row?;
    Ok(row.as_ref().map(log_line_from))
}

/// One process by id.
pub async fn process(data_root: &Path, process_id: &str) -> anyhow::Result<Option<ProcessRow>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(None);
    };
    let row = sql!(
        "runs.read_process",
        "SELECT process_id, process, run_id, invocation_id, step, attempt, started_at_utc, \
           finished_at_utc, exit_code, signal, tz_offset, git_hash, applet, peak_rss_bytes \
         FROM processes WHERE process_id = ?"
    )
    .query()
    .bind(process_id)
    .fetch_optional(&pool)
    .await;
    pool.close().await;
    let row = row?;
    Ok(row.as_ref().map(process_row_from))
}

fn process_row_from(r: &sqlx::sqlite::SqliteRow) -> ProcessRow {
    ProcessRow {
        process_id: r.get("process_id"),
        process: r.get("process"),
        run_id: r.get("run_id"),
        invocation_id: r.get("invocation_id"),
        step: r.get("step"),
        attempt: r.get("attempt"),
        started_at_utc: r.get("started_at_utc"),
        finished_at_utc: r.get("finished_at_utc"),
        exit_code: r.get("exit_code"),
        signal: r.get("signal"),
        tz_offset: r.get("tz_offset"),
        git_hash: r.get("git_hash"),
        applet: r.get("applet"),
        peak_rss_bytes: r.get("peak_rss_bytes"),
    }
}

/// The log lines of one request's or job's invocations, or of one older
/// build's run, after `after_seq`, oldest first, at most `limit`. `step`
/// narrows to one step. This is the tail: a client keeps the last `seq`
/// it saw and asks again.
pub async fn log_after(
    data_root: &Path,
    run: &str,
    step: Option<&str>,
    after_seq: i64,
    limit: i64,
) -> anyhow::Result<Vec<LogLine>> {
    let Some(pool) = open_reader(data_root).await? else {
        return Ok(Vec::new());
    };
    // Audited: the column list is this module's own constant.
    let rows = Sql::dynamic(
        sql_id!("runs.tail_log_lines"),
        format!(
            "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
             WHERE (l.run_id = ?1 OR l.invocation_id IN (SELECT invocation_id \
               FROM invocation_requests INDEXED BY invocation_requests_by_request \
               WHERE request_id = ?1)) \
             AND l.seq > ?2 AND (?3 IS NULL OR l.step = ?3) \
             ORDER BY l.seq LIMIT ?4"
        ),
    )
    .query()
    .bind(run)
    .bind(after_seq)
    .bind(step)
    .bind(limit)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = rows?;
    Ok(rows.iter().map(log_line_from).collect())
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
pub(crate) const LOG_LINE_COLUMNS: &str = "l.seq, l.run_id, l.invocation_id, l.process_id, \
     l.step, l.group_id, l.attempt, l.ts_utc, l.tz_offset, l.stream, l.level, l.target, \
     l.thread, l.msg, l.fields, p.process, p.git_hash";

pub(crate) fn log_line_from(r: &sqlx::sqlite::SqliteRow) -> LogLine {
    LogLine {
        row: LogRow {
            seq: r.get("seq"),
            run_id: r.get("run_id"),
            invocation_id: r.get("invocation_id"),
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

/// What names one metric series: its invocation, its step, its name and
/// its labels.
type SeriesKey = (String, String, String, String);

fn series_key(m: &InvocationMetricRow) -> SeriesKey {
    (
        m.invocation_id.clone(),
        m.step.clone(),
        m.name.clone(),
        m.labels.clone(),
    )
}

/// What has been published and not yet written. Invocation and metric
/// updates coalesce to the newest; log lines never do.
#[derive(Default)]
struct Pending {
    invocations: BTreeMap<String, InvocationRow>,
    served: Vec<InvocationRequestRow>,
    /// Each attempt of each invocation is its own process: started, then
    /// the same row again with its end. Newest wins.
    processes: BTreeMap<String, ProcessRow>,
    logs: Vec<LogRow>,
    metrics: BTreeMap<SeriesKey, InvocationMetricRow>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.invocations.is_empty()
            && self.served.is_empty()
            && self.processes.is_empty()
            && self.logs.is_empty()
            && self.metrics.is_empty()
    }
}

type Shared = Arc<Mutex<Pending>>;

/// Told the store's path each time a writer in this process has written
/// to it. The filesystem reports the same write, but late when it likes:
/// fseventsd has held events back for over a minute on a busy disk, and
/// the server's log card waits on that report to read its own lines.
type AfterWrite = Box<dyn Fn(&Path) + Send + Sync>;
static AFTER_WRITE: Mutex<Vec<AfterWrite>> = Mutex::new(Vec::new());

/// Calls `hook` after every write any writer in this process makes, from
/// now on, with the path of the store written. Writers started earlier
/// call it too.
pub fn after_every_write(hook: impl Fn(&Path) + Send + Sync + 'static) {
    AFTER_WRITE
        .lock()
        .expect("run store hook mutex")
        .push(Box::new(hook));
}

fn wrote(path: &Path) {
    for hook in AFTER_WRITE.lock().expect("run store hook mutex").iter() {
        hook(path);
    }
}

/// Something that takes log lines. Both writers below are one, which is
/// what lets the tracing layer feed either.
pub trait LogSink: Send + Sync {
    fn log(&self, row: LogRow);
}

/// Sent to the writer thread to have it flush now, and called once that
/// flush is over.
type FlushAck = Box<dyn FnOnce() + Send>;

/// The thread behind a writer, and the handles that stop it. Dropping
/// the sender tells the thread to flush once more and exit, which is
/// what makes the final rows land. It must be dropped *before* joining
/// — joining while still holding the sender waits forever on a thread
/// that has not been told to stop.
struct Writer {
    process_id: String,
    pending: Shared,
    redact: Redactor,
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
            redact: Redactor::from_env(),
            flushes: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        })
    }

    fn log(&self, row: LogRow) {
        let row = self.redact.log(row);
        self.pending.lock().expect("run store mutex").logs.push(row);
    }

    fn process(&self, row: ProcessRow) {
        self.pending
            .lock()
            .expect("run store mutex")
            .processes
            .insert(row.process_id.clone(), row);
    }

    async fn flush(&self) {
        let (ack, flushed) = tokio::sync::oneshot::channel();
        // A thread that has gone drops the ack, which ends the wait as
        // surely as an answer.
        if self.ask_to_flush(Box::new(move || {
            let _ = ack.send(());
        })) {
            let _ = flushed.await;
        }
    }

    /// [`Writer::flush`], for a caller that cannot await: it blocks the
    /// thread for one flush, a few milliseconds.
    fn flush_blocking(&self) {
        let (ack, flushed) = mpsc::sync_channel(1);
        if self.ask_to_flush(Box::new(move || {
            let _ = ack.send(());
        })) {
            let _ = flushed.recv();
        }
    }

    fn ask_to_flush(&self, ack: FlushAck) -> bool {
        self.flushes
            .lock()
            .expect("run store flush mutex")
            .as_ref()
            .is_some_and(|tx| tx.send(ack).is_ok())
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

/// Publishes what the loop's invocations do to the store, for as long as
/// the loop runs. Cheap to call; the work happens on its own thread. The
/// runner's own process is one row, and a line handed over with no
/// `process_id` is filed under it.
pub struct LoopWriter(Writer);

impl LoopWriter {
    /// `git_hash` is the commit the runner came from ([`crate::git_hash`]),
    /// or `None` when a dev build cannot say.
    pub fn start(data_root: &Path, git_hash: Option<String>, retention: Retention) -> Option<Self> {
        let clock = RunClock::starting_at(datalib_time::IsoOffsetTimestamp::now_local());
        let (started_at_utc, tz_offset) = clock.now().to_utc_and_offset();
        let process = ProcessRow {
            process_id: mint_process_id(),
            process: Process::Dag.as_str().into(),
            started_at_utc,
            tz_offset: Some(tz_offset),
            git_hash,
            ..Default::default()
        };
        Writer::start(
            data_root,
            Scope {
                process,
                retention,
                clock,
            },
        )
        .map(Self)
    }

    /// An invocation's whole row, as it starts and each time it moves. A
    /// finished row latches: a progress update already in flight when the
    /// invocation ended must not bring it back as running.
    pub fn invocation(&self, next: InvocationRow) {
        let next = self.0.redact.invocation(next);
        let mut p = self.0.pending.lock().expect("run store mutex");
        if let Some(prev) = p.invocations.get(&next.invocation_id) {
            if is_terminal(&prev.state) && !is_terminal(&next.state) {
                return;
            }
        }
        p.invocations.insert(next.invocation_id.clone(), next);
    }

    /// The requests or the job an invocation was started for.
    pub fn served(&self, invocation_id: &str, requests: &[String]) {
        let mut p = self.0.pending.lock().expect("run store mutex");
        for request in requests {
            p.served.push(InvocationRequestRow {
                invocation_id: invocation_id.to_string(),
                request_id: request.clone(),
            });
        }
    }

    pub fn log(&self, row: LogRow) {
        self.0.log(row);
    }

    pub fn metric(&self, row: InvocationMetricRow) {
        self.0
            .pending
            .lock()
            .expect("run store mutex")
            .metrics
            .insert(series_key(&row), row);
    }

    /// An attempt of an invocation, as it starts, and again as it ends
    /// with `finished_at_utc` and the exit filled in.
    pub fn process(&self, row: ProcessRow) {
        self.0.process(row);
    }

    /// Returns once everything published before the call is committed,
    /// or lost with a warning.
    pub async fn flush(&self) {
        self.0.flush().await;
    }

    /// [`LoopWriter::flush`], blocking the thread rather than awaiting:
    /// for the loop saying it is idle, which a reader of the store takes
    /// as "what ran is written".
    pub fn flush_blocking(&self) {
        self.0.flush_blocking();
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

impl LogSink for LoopWriter {
    fn log(&self, row: LogRow) {
        self.0.log(row);
    }
}

/// Publishes the log of a process that runs no invocation — the app's
/// server, or a `datalib-pipeline` verb's own tracing — to the same
/// table, about no invocation. `git_hash` is the commit the process came
/// from ([`crate::git_hash`]).
pub struct ProcessLogWriter(Writer);

impl ProcessLogWriter {
    pub fn start(
        data_root: &Path,
        process: Process,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Option<Self> {
        let clock = RunClock::starting_at(datalib_time::IsoOffsetTimestamp::now_local());
        let (started_at_utc, tz_offset) = clock.now().to_utc_and_offset();
        let tz_offset = Some(tz_offset);
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
                retention,
                clock,
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
        self.0.process(row);
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

/// What a writer thread is writing on behalf of: the process it is.
struct Scope {
    process: ProcessRow,
    retention: Retention,
    /// What the process's end is stamped from.
    clock: RunClock,
}

fn mint_process_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// What the writer thread remembers per metric series, to decide when
/// a sample is due.
struct SeriesState {
    last_sample_at: Instant,
    last_sample_value: i64,
    current: InvocationMetricRow,
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
    match rt.block_on(begin(&pool, &scope)) {
        Ok(()) => wrote(&path),
        Err(e) => tracing::warn!(error = %e, "run store: could not record the start"),
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
        match rt.block_on(flush_or_retry(&pool, &scope, &batch, &mut series, done)) {
            Ok(()) if !batch.is_empty() => wrote(&path),
            Ok(()) => {}
            // One bad flush must not stop the recording for every other
            // step, and must never take the loop down. Say how many
            // lines went with it: the count is the only trace left of
            // them, and a silent loss here is what makes anyone
            // distrust the store.
            Err(e) => {
                tracing::warn!(error = %e, lines, "run store: write failed twice; the batch is lost")
            }
        }
        if let Some(ack) = ack {
            ack();
        }
        if done {
            break;
        }
        if last_prune.elapsed() >= PRUNE_EVERY {
            if let Err(e) = rt.block_on(prune(&pool, scope.retention)) {
                tracing::warn!(error = %e, "run store: could not prune old history");
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

/// LoopState the process and apply retention.
async fn begin(pool: &SqlitePool, scope: &Scope) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if insert_process(&mut tx, &scope.process).await? {
        bump(&mut tx, StorePart::Processes).await?;
    }
    tx.commit().await?;
    prune(pool, scope.retention).await
}

/// Idempotent, and called again with every batch of lines: a process
/// that outlives the age cutoff and has no lines left in the store
/// is pruned, and must not then write lines that name no process.
/// True when the row is new: a watcher hears of that, not of a repeat.
async fn insert_process(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    p: &ProcessRow,
) -> Result<bool, sqlx::Error> {
    let inserted = sql!(
        "runs.insert_process",
        "INSERT OR IGNORE INTO processes \
           (process_id, process, run_id, invocation_id, step, attempt, started_at_utc, \
            finished_at_utc, exit_code, signal, tz_offset, git_hash, applet, peak_rss_bytes) \
         VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?, ?, ?)"
    )
    .query()
    .bind(&p.process_id)
    .bind(&p.process)
    .bind(&p.invocation_id)
    .bind(&p.step)
    .bind(p.attempt)
    .bind(&p.started_at_utc)
    .bind(&p.tz_offset)
    .bind(&p.git_hash)
    .bind(&p.applet)
    .bind(p.peak_rss_bytes)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;
    Ok(inserted)
}

/// A process another one reports — a step's attempt by the runner, an
/// applet or a page by the server: the whole row at start, again as its
/// peak memory rises, and with its end and exit once it is over. A row
/// that names no peak keeps the one already stored.
async fn upsert_process(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    p: &ProcessRow,
) -> Result<(), sqlx::Error> {
    sql!(
        "runs.upsert_process",
        "INSERT INTO processes \
           (process_id, process, run_id, invocation_id, step, attempt, started_at_utc, \
            finished_at_utc, exit_code, signal, tz_offset, git_hash, applet, peak_rss_bytes) \
         VALUES (?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(process_id) DO UPDATE SET finished_at_utc = excluded.finished_at_utc, \
           exit_code = excluded.exit_code, signal = excluded.signal, \
           git_hash = COALESCE(excluded.git_hash, processes.git_hash), \
           peak_rss_bytes = COALESCE(excluded.peak_rss_bytes, processes.peak_rss_bytes)"
    )
    .query()
    .bind(&p.process_id)
    .bind(&p.process)
    .bind(&p.invocation_id)
    .bind(&p.step)
    .bind(p.attempt)
    .bind(&p.started_at_utc)
    .bind(&p.finished_at_utc)
    .bind(p.exit_code)
    .bind(p.signal)
    .bind(&p.tz_offset)
    .bind(&p.git_hash)
    .bind(&p.applet)
    .bind(p.peak_rss_bytes)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Close every invocation the store still reads as live, for the process
/// that has just taken `runner-lock`: holding it, no invocation is live,
/// so any that reads so was left by a loop that died. Its state becomes
/// `state`, the caller's word (this crate does not name the runner's), and
/// `why` goes on its error. How many it closed.
///
/// Processes are left as they are: this crate would have to invent an
/// exit for them, and "we never found out" is the honest answer.
pub async fn close_abandoned_invocations(
    data_root: &Path,
    state: &str,
    why: &str,
) -> Result<u64, sqlx::Error> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Ok(0);
    }
    let pool = open_existing(&path).await?;
    let out = close_abandoned_in(&pool, state, why).await;
    pool.close().await;
    out
}

async fn close_abandoned_in(pool: &SqlitePool, state: &str, why: &str) -> Result<u64, sqlx::Error> {
    let (at, tz_offset) = now_split();
    // The live states, from the one place that names them.
    let live: Vec<&'static str> = <LiveState as strum::VariantArray>::VARIANTS
        .iter()
        .map(|s| s.as_str())
        .collect();
    let mut tx = pool.begin().await?;
    let closed = sql!(
        "runs.close_abandoned_invocations",
        // `max`: an invocation cannot end before it began, whatever the
        // two clocks say.
        "UPDATE invocations SET state = ?1, \
           finished_at_utc = coalesce(finished_at_utc, max(?2, started_at_utc)), \
           error = coalesce(error, ?3), updated_at_utc = ?2, tz_offset = coalesce(tz_offset, ?4) \
         WHERE state IN (SELECT value FROM json_each(?5))"
    )
    .query()
    .bind(state)
    .bind(&at)
    .bind(why)
    .bind(&tz_offset)
    .bind(serde_json::Value::from(live).to_string())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if closed > 0 {
        bump(&mut tx, StorePart::Invocations).await?;
    }
    tx.commit().await?;
    Ok(closed)
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
    sql!(
        "runs.bump_store_change",
        "INSERT INTO store_changes (what, version, changed_at_utc, tz_offset) VALUES (?, 1, ?, ?) \
         ON CONFLICT(what) DO UPDATE SET version = store_changes.version + 1, \
           changed_at_utc = excluded.changed_at_utc, tz_offset = excluded.tz_offset"
    )
    .query()
    .bind(what.as_str())
    .bind(changed_at_utc)
    .bind(tz_offset)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Retention, at every writer's start and every [`PRUNE_EVERY`] after:
/// the invocations, an older build's runs, and what belongs to neither.
async fn prune(pool: &SqlitePool, retention: Retention) -> Result<(), sqlx::Error> {
    prune_invocations(pool, retention).await?;
    prune_old_runs(pool, retention).await?;
    prune_unowned(pool, retention).await
}

/// An invocation goes once it is older than `max_age_days`, or once none
/// of the newest `max_runs` requests and jobs it served is kept; one still
/// running stays. Its links, metrics, samples, processes and lines go with
/// it, and so do the samples the runner kept for steps between
/// invocations once they are as old.
async fn prune_invocations(pool: &SqlitePool, retention: Retention) -> Result<(), sqlx::Error> {
    let cutoff = cutoff_days_ago(retention.max_age_days);
    let mut tx = pool.begin().await?;
    let mut gone: Vec<String> = sql!(
        "runs.delete_invocations_past_age",
        "DELETE FROM invocations INDEXED BY invocations_by_start \
         WHERE started_at_utc < ? AND finished_at_utc IS NOT NULL RETURNING invocation_id"
    )
    .query_scalar()
    .bind(&cutoff)
    .fetch_all(&mut *tx)
    .await?;
    let past_count: Vec<String> = sql!(
        "runs.delete_invocations_past_max",
        // Marked: it reads every link to rank the requests, once an hour
        // (`datalib_sql_work::judge::BY_DESIGN`).
        "DELETE FROM invocations /* walks by design */ \
         WHERE finished_at_utc IS NOT NULL AND invocation_id NOT IN ( \
           SELECT q.invocation_id FROM invocation_requests q WHERE q.request_id IN ( \
             SELECT k.request_id FROM invocation_requests k \
               JOIN invocations i ON i.invocation_id = k.invocation_id \
             GROUP BY k.request_id ORDER BY MAX(i.started_at_utc) DESC LIMIT ?)) \
         RETURNING invocation_id"
    )
    .query_scalar()
    .bind(i64::from(retention.max_runs.max(1)))
    .fetch_all(&mut *tx)
    .await?;
    gone.extend(past_count);
    if !gone.is_empty() {
        let gone = serde_json::to_string(&gone).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        const BY_INVOCATION: &[&str] = &[
            // These four are keyed by `(invocation_id, …)`: their key is
            // the index.
            "DELETE FROM invocation_requests WHERE invocation_id IN (SELECT value FROM json_each(?))",
            "DELETE FROM invocation_metrics WHERE invocation_id IN (SELECT value FROM json_each(?))",
            "DELETE FROM invocation_metric_samples \
             WHERE invocation_id IN (SELECT value FROM json_each(?))",
            "DELETE FROM log INDEXED BY log_by_invocation \
             WHERE invocation_id IN (SELECT value FROM json_each(?))",
            "DELETE FROM processes INDEXED BY processes_by_invocation \
             WHERE invocation_id IN (SELECT value FROM json_each(?))",
        ];
        for statement in BY_INVOCATION {
            Sql::dynamic(sql_id!("runs.delete_rows_of_invocations"), *statement)
                .query()
                .bind(&gone)
                .execute(&mut *tx)
                .await?;
        }
    }
    sql!(
        "runs.delete_old_kept_samples",
        "DELETE FROM invocation_metric_samples WHERE invocation_id = '' AND ts_utc < ?"
    )
    .query()
    .bind(&cutoff)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// An older build's runs, by the same two limits as invocations.
async fn prune_old_runs(pool: &SqlitePool, retention: Retention) -> Result<(), sqlx::Error> {
    let cutoff = cutoff_days_ago(retention.max_age_days);
    let mut tx = pool.begin().await?;
    let mut gone: Vec<String> = sql!(
        "runs.delete_runs_past_retention",
        "DELETE FROM runs INDEXED BY runs_by_start WHERE started_at_utc < ? RETURNING run_id"
    )
    .query_scalar()
    .bind(&cutoff)
    .fetch_all(&mut *tx)
    .await?;
    let past_count: Vec<String> = sql!(
        "runs.delete_runs_past_max",
        "DELETE FROM runs INDEXED BY runs_by_start \
         WHERE (started_at_utc, run_id) < \
           (SELECT started_at_utc, run_id FROM runs INDEXED BY runs_by_start \
            ORDER BY started_at_utc DESC, run_id DESC LIMIT 1 OFFSET ?) \
         RETURNING run_id"
    )
    .query_scalar()
    .bind(i64::from(retention.max_runs.max(1)) - 1)
    .fetch_all(&mut *tx)
    .await?;
    gone.extend(past_count);
    if !gone.is_empty() {
        delete_rows_of_runs(&mut tx, &gone).await?;
    }
    tx.commit().await
}

/// What every table filed under a run holds of `runs`, each read through
/// the index or key its `run_id` leads, so retention costs the rows it
/// removes rather than every line the store holds.
async fn delete_rows_of_runs(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    runs: &[String],
) -> Result<(), sqlx::Error> {
    const BY_RUN: &[&str] = &[
        "DELETE FROM log INDEXED BY log_by_run_step \
         WHERE run_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM processes INDEXED BY processes_by_run \
         WHERE run_id IN (SELECT value FROM json_each(?))",
        // These three are keyed by `(run_id, …)`: their key is the index.
        "DELETE FROM step_runs WHERE run_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM metrics WHERE run_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM metric_samples WHERE run_id IN (SELECT value FROM json_each(?))",
    ];
    let runs = serde_json::to_string(runs).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    for sql in BY_RUN {
        Sql::dynamic(sql_id!("runs.delete_rows_of_runs"), *sql)
            .query()
            .bind(&runs)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Retention for what belongs to no invocation and no old run: the
/// runner's and the server's own lines have their own, shorter age and a
/// row cap — a server at `debug` between syncs must not grow the file for
/// a month — and a process goes once it is past that age with nothing
/// left to name it.
async fn prune_unowned(pool: &SqlitePool, retention: Retention) -> Result<(), sqlx::Error> {
    let cutoff = cutoff_days_ago(retention.process_log_days);
    // Marked: each counts every line it keeps, once per open
    // (`datalib_sql_work::judge::BY_DESIGN`).
    sql!(
        "runs.delete_old_unowned_log",
        "DELETE FROM log /* walks by design */ \
         WHERE run_id IS NULL AND invocation_id IS NULL AND ts_utc < ?"
    )
    .query()
    .bind(&cutoff)
    .execute(pool)
    .await?;
    sql!(
        "runs.delete_excess_unowned_log",
        "DELETE FROM log /* walks by design */ \
         WHERE run_id IS NULL AND invocation_id IS NULL AND seq < \
         (SELECT seq FROM log WHERE run_id IS NULL AND invocation_id IS NULL \
          ORDER BY seq DESC LIMIT 1 OFFSET ?)"
    )
    .query()
    .bind(retention.process_log_lines.max(1) as i64 - 1)
    .execute(pool)
    .await?;
    sql!(
        "runs.delete_old_unowned_processes",
        "DELETE FROM processes WHERE run_id IS NULL AND invocation_id IS NULL \
         AND started_at_utc < ? AND process_id NOT IN (SELECT process_id FROM log)"
    )
    .query()
    .bind(&cutoff)
    .execute(pool)
    .await?;
    Ok(())
}

fn cutoff_days_ago(days: u32) -> String {
    datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(days as i64) * 86_400 * 1_000_000)
        .to_utc_and_offset()
        .0
}

/// The process is over. Counted as a change to the processes, so a
/// watcher redraws its row in the log's picker.
async fn end(pool: &SqlitePool, scope: &Scope) -> Result<(), sqlx::Error> {
    let (finished_at_utc, _) = scope.clock.now().to_utc_and_offset();
    let mut tx = pool.begin().await?;
    sql!(
        "runs.mark_process_finished",
        "UPDATE processes SET finished_at_utc = ? WHERE process_id = ?"
    )
    .query()
    .bind(&finished_at_utc)
    .bind(&scope.process.process_id)
    .execute(&mut *tx)
    .await?;
    bump(&mut tx, StorePart::Processes).await?;
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
    let now = Instant::now();
    // An invocation that ended in this batch has its series' last values
    // sampled, as the writer's last flush samples every series.
    let ended: BTreeSet<&str> = batch
        .invocations
        .values()
        .filter(|i| is_terminal(&i.state))
        .map(|i| i.invocation_id.as_str())
        .collect();
    let plan = plan_series(series, &batch.metrics, now, last, &ended);
    if batch.is_empty() && !last && plan.samples.is_empty() {
        return Ok(());
    }
    let own_process_id = scope.process.process_id.as_str();
    let mut tx = pool.begin().await?;
    for p in batch.processes.values() {
        upsert_process(&mut tx, p).await?;
    }
    let mut processes_moved = !batch.processes.is_empty();
    if !batch.invocations.is_empty() || !batch.served.is_empty() {
        bump(&mut tx, StorePart::Invocations).await?;
    }
    if !batch.logs.is_empty() {
        processes_moved |= insert_process(&mut tx, &scope.process).await?;
        if batch.logs.iter().any(|l| l.invocation_id.is_some()) {
            bump(&mut tx, StorePart::InvocationLog).await?;
        }
        if batch.logs.iter().any(|l| l.invocation_id.is_none()) {
            bump(&mut tx, StorePart::ProcessLog).await?;
        }
    }
    if !batch.metrics.is_empty() {
        bump(&mut tx, StorePart::Metrics).await?;
    }
    if processes_moved {
        bump(&mut tx, StorePart::Processes).await?;
    }
    for i in batch.invocations.values() {
        sql!(
            "runs.upsert_invocation",
            "INSERT INTO invocations (invocation_id, step, group_id, verb, state, attempt, \
               started_at_utc, finished_at_utc, now, fingerprint, reads, error, msg, exit_code, \
               signal, updated_at_utc, tz_offset) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(invocation_id) DO UPDATE SET \
               state = excluded.state, attempt = excluded.attempt, \
               finished_at_utc = excluded.finished_at_utc, error = excluded.error, \
               msg = excluded.msg, exit_code = excluded.exit_code, signal = excluded.signal, \
               updated_at_utc = excluded.updated_at_utc, tz_offset = excluded.tz_offset"
        )
        .query()
        .bind(&i.invocation_id)
        .bind(&i.step)
        .bind(&i.group_id)
        .bind(&i.verb)
        .bind(&i.state)
        .bind(i.attempt)
        .bind(&i.started_at_utc)
        .bind(&i.finished_at_utc)
        .bind(&i.now)
        .bind(&i.fingerprint)
        .bind(&i.reads)
        .bind(&i.error)
        .bind(&i.msg)
        .bind(i.exit_code)
        .bind(i.signal)
        .bind(&i.updated_at_utc)
        .bind(&i.tz_offset)
        .execute(&mut *tx)
        .await?;
    }
    for s in &batch.served {
        sql!(
            "runs.insert_invocation_request",
            "INSERT OR IGNORE INTO invocation_requests (invocation_id, request_id) VALUES (?, ?)"
        )
        .query()
        .bind(&s.invocation_id)
        .bind(&s.request_id)
        .execute(&mut *tx)
        .await?;
    }
    for l in &batch.logs {
        let process_id = if l.process_id.is_empty() {
            own_process_id
        } else {
            l.process_id.as_str()
        };
        sql!(
            "runs.insert_log_line",
            "INSERT INTO log (run_id, invocation_id, process_id, step, group_id, attempt, ts_utc, \
                              tz_offset, stream, level, target, thread, msg, fields) \
             VALUES (NULL,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        )
        .query()
        .bind(&l.invocation_id)
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
        sql!(
            "runs.upsert_invocation_metric",
            "INSERT INTO invocation_metrics \
               (invocation_id, step, name, labels, value, updated_at_utc, tz_offset) \
             VALUES (?,?,?,?,?,?,?) \
             ON CONFLICT(invocation_id, step, name, labels) DO UPDATE SET \
               value = excluded.value, updated_at_utc = excluded.updated_at_utc, \
               tz_offset = excluded.tz_offset"
        )
        .query()
        .bind(&m.invocation_id)
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
        insert_sample(&mut tx, m).await?;
    }
    tx.commit().await?;
    for (key, state) in plan.next {
        series.insert(key, state);
    }
    // An ended invocation's series write nothing more.
    series.retain(|(invocation, ..), _| !ended.contains(invocation.as_str()));
    Ok(())
}

/// What a flush is about to do to the sample series: the rows to write,
/// and the state to install once its transaction commits.
struct SeriesPlan {
    samples: Vec<InvocationMetricRow>,
    next: Vec<(SeriesKey, SeriesState)>,
}

/// A sample is due when a series has moved and the floor between
/// samples has passed. A move that came too soon after the last sample
/// is written once the floor has passed, even if the series has not
/// moved again — at the stamp it moved at: a queue that fills and then
/// empties within one floor would otherwise keep its peak in the history
/// until its next move. When an invocation in `ended` has just finished,
/// and on the writer's last flush, whatever each series of it ended at is
/// written however recent its previous sample: a rate drawn to the end
/// needs that point.
fn plan_series(
    series: &HashMap<SeriesKey, SeriesState>,
    batch: &BTreeMap<SeriesKey, InvocationMetricRow>,
    now: Instant,
    last: bool,
    ended: &BTreeSet<&str>,
) -> SeriesPlan {
    let closing_now = |key: &SeriesKey| last || ended.contains(key.0.as_str());
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
        let closing =
            closing_now(key) && !due && previous.is_some_and(|s| s.last_sample_value != m.value);
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
        if !held_back || !(closing_now(key) || now - s.last_sample_at >= SAMPLE_EVERY) {
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

async fn insert_sample(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    m: &InvocationMetricRow,
) -> Result<(), sqlx::Error> {
    sql!(
        "runs.upsert_invocation_metric_sample",
        "INSERT OR REPLACE INTO invocation_metric_samples \
           (invocation_id, step, name, labels, ts_utc, tz_offset, value) VALUES (?,?,?,?,?,?,?)"
    )
    .query()
    .bind(&m.invocation_id)
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

    use datalib_sql::test_sql;
    /// The run store's `-wal` stays within its size limit with no
    /// checkpoint of ours: the engine checkpoints as it goes. Under
    /// doltlite 0.51.3, which never did, a live root's `-wal` reached 4 GB
    /// over eight days, behind which every read of the log took seconds.
    #[tokio::test]
    async fn the_wal_stays_within_its_limit_by_itself() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("runs.sqlite");
        let pool = open_or_create(&path).await.unwrap();
        test_sql!("CREATE TABLE filler (b BLOB)")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        // About 80 MB in 20 commits, past the 64 MB limit.
        for _ in 0..20 {
            test_sql!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) \
                 INSERT INTO filler SELECT randomblob(4000) FROM n"
            )
            .query()
            .execute(&pool)
            .await
            .unwrap();
        }
        let wal = std::fs::metadata(path.with_file_name("runs.sqlite-wal"))
            .unwrap()
            .len();
        let limit: u64 = datalib_runtime::plain_sqlite::WAL_SIZE_LIMIT_BYTES
            .parse()
            .unwrap();
        assert!(wal <= limit, "the -wal grew to {wal} bytes");
        pool.close().await;
    }

    /// A queue that fills and then empties within the sample floor: the
    /// drop is not written at first, then written once the floor has
    /// passed — with nothing new in the batch, and at the stamp it happened — rather
    /// than leaving the peak in the history until the queue next moves.
    #[test]
    fn a_move_too_soon_after_the_last_sample_is_written_once_the_floor_passes() {
        let key: SeriesKey = ("i".into(), "s".into(), "queued".into(), String::new());
        let row = |value: i64, at: &str| InvocationMetricRow {
            invocation_id: "i".into(),
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
        let batch = |m: InvocationMetricRow| BTreeMap::from([(key.clone(), m)]);
        let none = BTreeSet::new();

        let first = plan_series(&series, &batch(row(50, "t1")), t0, false, &none);
        assert_eq!(apply(&mut series, first), [(50, "t1".to_string())]);
        let inside = plan_series(
            &series,
            &batch(row(0, "t2")),
            t0 + Duration::from_secs(1),
            false,
            &none,
        );
        assert_eq!(apply(&mut series, inside), [], "inside the floor");
        let quiet = plan_series(
            &series,
            &BTreeMap::new(),
            t0 + Duration::from_secs(3),
            false,
            &none,
        );
        assert_eq!(apply(&mut series, quiet), [], "still inside the floor");
        let after = plan_series(&series, &BTreeMap::new(), t0 + SAMPLE_EVERY, false, &none);
        assert_eq!(apply(&mut series, after), [(0, "t2".to_string())]);
        let again = plan_series(
            &series,
            &BTreeMap::new(),
            t0 + SAMPLE_EVERY * 3,
            false,
            &none,
        );
        assert_eq!(apply(&mut series, again), [], "written once");
    }

    /// A store from another schema version, with a reader that opened
    /// it before a writer remade it — `datalib-http` opens one per
    /// read. When the remake unlinked the file, the reader stayed on
    /// the old inode, took the new file's journal for a crashed
    /// writer's, rolled it back and deleted it, and the writer's commit
    /// failed with 5898 (`SQLITE_IOERR_DELETE_NOENT`). CI run
    /// 36441513702 lost a whole writer's lines this way. In WAL mode
    /// doltlite's reset waits for the reader to close, as a read does
    /// within milliseconds; then a reader mid-transaction must not
    /// cost the writer its commit.
    #[tokio::test]
    async fn a_reader_open_across_the_remake_leaves_the_writers_journal_alone() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("runs.sqlite");
        let planted = open_or_create(&path).await.unwrap();
        test_sql!("CREATE TABLE ancient (x TEXT); PRAGMA user_version = 1")
            .raw()
            .execute(&planted)
            .await
            .unwrap();
        planted.close().await;

        let reader = open_existing(&path).await.unwrap();
        let tables = test_sql!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'");
        tables
            .query_scalar::<i64>()
            .fetch_one(&reader)
            .await
            .unwrap();

        let remake = tokio::spawn({
            let path = path.clone();
            async move { open_or_recreate(&path).await }
        });
        // The copy is the step before the reset, so the remake is at
        // the reset when the reader goes.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_dir(td.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("runs.bak_")
        }) {
            assert!(Instant::now() < deadline, "the remake never kept a copy");
            tokio::task::yield_now().await;
        }
        reader.close().await;
        let writer = remake
            .await
            .unwrap()
            .expect("the remake, once the reader closed");

        let reader = open_existing(&path).await.unwrap();
        let mut tx = writer.begin().await.unwrap();
        test_sql!(
            "INSERT INTO log (process_id, attempt, ts_utc, level, msg) \
             VALUES ('p', 0, 't', 'info', 'kept')"
        )
        .query()
        .execute(&mut *tx)
        .await
        .unwrap();
        tables
            .query_scalar::<i64>()
            .fetch_one(&reader)
            .await
            .unwrap();
        tx.commit()
            .await
            .expect("the writer's commit, after a reader read mid-transaction");

        let kept: Vec<String> = test_sql!("SELECT msg FROM log")
            .query_scalar()
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
