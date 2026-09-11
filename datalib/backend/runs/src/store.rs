//! Writing the store, and reading it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::{is_terminal, runs_path, Retention, SCHEMA, SCHEMA_VERSION};

/// How often the writer thread flushes. 200ms is under the threshold
/// where a progress display reads as laggy, and far above the cost of
/// the write (~0.3ms per row on a plain-SQLite file, measured).
const FLUSH_EVERY: Duration = Duration::from_millis(200);

/// The floor between two samples of one metric series. A series that
/// changes every flush would otherwise write five rows a second for as
/// long as the step runs; a rate drawn from five-second samples is the
/// same rate.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

fn connect_string(path: &Path) -> String {
    // Percent-encode only what would otherwise terminate the path or be
    // decoded away. SQLite percent-decodes the path portion of a URI, so
    // a bare `%` in a directory name would eat the next two characters.
    // Spaces are left alone — SQLite accepts them, and data roots have
    // them (this repo lives under one).
    let escaped = path
        .display()
        .to_string()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{escaped}?doltlite_engine=sqlite")
}

fn options(path: &Path, create: bool) -> SqliteConnectOptions {
    // `filename`, not `from_str`: sqlx's *URL parser* rejects query
    // parameters it does not recognise ("unknown query parameter
    // `doltlite_engine`"), while the filename field is handed to
    // `sqlite3_open_v2` untouched. The URI has to go in through the door
    // sqlx does not inspect.
    SqliteConnectOptions::new()
        .filename(connect_string(path))
        .create_if_missing(create)
        // WAL so a reader never blocks behind the writer. On a plain
        // file this is real, unlike on a doltlite one where it is a
        // documented no-op.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // Nothing here is load-bearing, so nothing is worth an fsync. A
        // file torn by a power cut is deleted and remade on the next
        // open (see `open_or_recreate`).
        .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
}

pub async fn open_or_create(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(sqlx::Error::Io)?;
    }
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options(path, true))
        .await
}

/// Open an existing store read-only-ish. Never creates: a reader that
/// created the file would race the runner for which engine claims it.
async fn open_existing(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options(path, false))
        .await
}

fn remove_with_sidecars(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// Open the store and make sure its schema is there, replacing a file
/// that will not open or was written by another schema version.
/// `synchronous=Off` means an OS crash can leave an unreadable file
/// behind, and losing old logs is a better outcome than a run that
/// refuses to start.
async fn open_or_recreate(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let why = match open_or_create(path).await {
        Ok(pool) => match schema_matches(&pool).await {
            Ok(true) => return Ok(pool),
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
    tracing::warn!(
        path = %path.display(),
        why,
        "run store: replacing the file"
    );
    remove_with_sidecars(path);
    let pool = open_or_create(path).await?;
    install_schema(&pool).await?;
    Ok(pool)
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

async fn install_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    // Safe: a compile-time integer, not input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {SCHEMA_VERSION}"
    )))
    .execute(pool)
    .await?;
    Ok(())
}

/// One step's state in one run, as a reader sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StepRow {
    pub step: String,
    /// A [`crate::LiveState`], or the terminal status the scheduler gave it.
    pub state: String,
    /// Invocations so far this run; 0 before the first.
    pub attempt: u32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    /// The step's own words: "conversations.list", "3 of 9 channels".
    pub msg: Option<String>,
    pub updated_at: String,
}

/// One log line. `seq` is assigned by the store; a writer leaves it 0.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LogRow {
    pub seq: i64,
    /// `None` for a line about the run rather than one step.
    pub step: Option<String>,
    /// Which invocation of the step within the run; 0 when unknown.
    pub attempt: u32,
    /// The line's own timestamp when it carried one, else when the
    /// runner saw it.
    pub ts: String,
    /// `stdout` or `stderr` for a subprocess's line; `None` for one the
    /// runner wrote.
    pub stream: Option<String>,
    pub level: String,
    /// The tracing target, when the line was structured tracing output.
    pub target: Option<String>,
    /// The thread that wrote it, when the line said.
    pub thread: Option<String>,
    pub msg: String,
    /// A JSON object of the structured fields beyond the message, when
    /// there were any.
    pub fields: Option<String>,
}

/// The current value of one metric series.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetricRow {
    pub step: String,
    pub name: String,
    /// The labels canonicalized to one string (`table=slack_messages`),
    /// empty for a series with none. See [`canonical_labels`].
    pub labels: String,
    pub value: i64,
    pub updated_at: String,
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

/// The newest run in the store, as a reader sees it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    /// Which run these rows describe. `None` for an empty or absent
    /// store. A reader comparing this against the run it is displaying is
    /// how it avoids painting one run's numbers onto another.
    pub run_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub steps: Vec<StepRow>,
    pub metrics: Vec<MetricRow>,
}

pub async fn snapshot(data_root: &Path) -> Snapshot {
    let path = runs_path(data_root);
    if !path.exists() {
        return Snapshot::default();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Snapshot::default();
    };
    let out = read_snapshot(&pool).await.unwrap_or_default();
    pool.close().await;
    out
}

async fn read_snapshot(pool: &SqlitePool) -> Result<Snapshot, sqlx::Error> {
    let Some(run) = sqlx::query(
        "SELECT run_id, started_at, finished_at FROM runs ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Snapshot::default());
    };
    let run_id: String = run.get("run_id");
    let steps = sqlx::query(
        "SELECT step, state, attempt, started_at, finished_at, error, msg, updated_at \
         FROM step_runs WHERE run_id = ? ORDER BY step",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| StepRow {
        step: r.get("step"),
        state: r.get("state"),
        attempt: r.get::<i64, _>("attempt") as u32,
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        error: r.get("error"),
        msg: r.get("msg"),
        updated_at: r.get("updated_at"),
    })
    .collect();
    let metrics = sqlx::query(
        "SELECT step, name, labels, value, updated_at FROM metrics \
         WHERE run_id = ? ORDER BY step, name, labels",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| MetricRow {
        step: r.get("step"),
        name: r.get("name"),
        labels: r.get("labels"),
        value: r.get("value"),
        updated_at: r.get("updated_at"),
    })
    .collect();
    Ok(Snapshot {
        run_id: Some(run_id),
        started_at: run.get("started_at"),
        finished_at: run.get("finished_at"),
        steps,
        metrics,
    })
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
) -> Vec<LogRow> {
    let path = runs_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Vec::new();
    };
    let rows = sqlx::query(
        "SELECT seq, step, attempt, ts, stream, level, target, thread, msg, fields FROM log \
         WHERE run_id = ? AND seq > ? AND (? IS NULL OR step = ?) \
         ORDER BY seq LIMIT ?",
    )
    .bind(run_id)
    .bind(after_seq)
    .bind(step)
    .bind(step)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    rows.iter()
        .map(|r| LogRow {
            seq: r.get("seq"),
            step: r.get("step"),
            attempt: r.get::<i64, _>("attempt") as u32,
            ts: r.get("ts"),
            stream: r.get("stream"),
            level: r.get("level"),
            target: r.get("target"),
            thread: r.get("thread"),
            msg: r.get("msg"),
            fields: r.get("fields"),
        })
        .collect()
}

/// What has been published and not yet written. Step and metric updates
/// coalesce to the newest; log lines never do.
#[derive(Default)]
struct Pending {
    steps: BTreeMap<String, StepRow>,
    logs: Vec<LogRow>,
    metrics: BTreeMap<(String, String, String), MetricRow>,
}

type Shared = Arc<Mutex<Pending>>;

/// Publishes one run to the store. Cheap to call; the work happens on
/// its own thread.
pub struct RunWriter {
    pending: Shared,
    /// Dropping this tells the writer thread to flush once more and
    /// exit, which is what makes a run's final states land. It must be
    /// dropped *before* joining — joining while still holding the
    /// sender waits forever on a thread that has not been told to stop.
    stop: Mutex<Option<mpsc::Sender<()>>>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RunWriter {
    pub fn start(
        data_root: &Path,
        run_id: &str,
        started_at: &str,
        retention: Retention,
    ) -> Option<Self> {
        let path = runs_path(data_root);
        let pending: Shared = Default::default();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("run-store".into())
            .spawn({
                let pending = pending.clone();
                let run = RunInfo {
                    run_id: run_id.to_string(),
                    started_at: started_at.to_string(),
                    retention,
                };
                move || writer_loop(path, run, pending, rx)
            })
            .ok()?;
        Some(Self {
            pending,
            stop: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        })
    }

    fn finish(&self) {
        // Sender first — that disconnect is the loop's exit signal.
        drop(self.stop.lock().expect("run store stop mutex").take());
        let handle = self.handle.lock().expect("run store handle mutex").take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    pub fn step(&self, next: StepRow) {
        let mut p = self.pending.lock().expect("run store mutex");
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
        self.pending.lock().expect("run store mutex").logs.push(row);
    }

    pub fn metric(&self, row: MetricRow) {
        let key = (row.step.clone(), row.name.clone(), row.labels.clone());
        self.pending
            .lock()
            .expect("run store mutex")
            .metrics
            .insert(key, row);
    }
}

impl Drop for RunWriter {
    fn drop(&mut self) {
        // The ordinary path. `finish` is what guarantees the final
        // flush landed before the process reports the run as finished.
        self.finish();
    }
}

struct RunInfo {
    run_id: String,
    started_at: String,
    retention: Retention,
}

/// What the writer thread remembers per metric series, to decide when
/// a sample is due.
struct SeriesState {
    last_sample_at: Instant,
    last_sample_value: i64,
    current: MetricRow,
}

fn writer_loop(path: PathBuf, run: RunInfo, pending: Shared, stop: mpsc::Receiver<()>) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let pool = match rt.block_on(open_or_recreate(&path)) {
        Ok(pool) => pool,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "run store: open failed; nothing recorded this run");
            return;
        }
    };
    if let Err(e) = rt.block_on(begin_run(&pool, &run)) {
        tracing::warn!(error = %e, "run store: could not record the run");
    }

    let mut series: HashMap<(String, String, String), SeriesState> = HashMap::new();
    loop {
        // Wake on the interval, or immediately when the writer is
        // dropped — whose disconnect is the signal to flush and go.
        let done = matches!(
            stop.recv_timeout(FLUSH_EVERY),
            Err(RecvTimeoutError::Disconnected) | Ok(())
        );
        let batch = {
            let mut p = pending.lock().expect("run store mutex");
            std::mem::take(&mut *p)
        };
        if let Err(e) = rt.block_on(flush(&pool, &run.run_id, batch, &mut series, done)) {
            // One bad flush must not stop the record for every other
            // step, and must never take the run down.
            tracing::warn!(error = %e, "run store: write failed");
        }
        if done {
            break;
        }
    }
    if let Err(e) = rt.block_on(end_run(&pool, &run.run_id)) {
        tracing::warn!(error = %e, "run store: could not close the run");
    }
    rt.block_on(pool.close());
}

fn now_rfc3339() -> String {
    datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339()
}

/// Record this run and apply retention. The run's own row goes in first
/// so the count limit includes it.
async fn begin_run(pool: &SqlitePool, run: &RunInfo) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runs (run_id, started_at) VALUES (?, ?) \
         ON CONFLICT(run_id) DO UPDATE SET started_at = excluded.started_at, finished_at = NULL",
    )
    .bind(&run.run_id)
    .bind(&run.started_at)
    .execute(&mut *tx)
    .await?;
    let cutoff = datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(run.retention.max_age_days as i64) * 86_400 * 1_000_000)
        .to_rfc3339();
    sqlx::query("DELETE FROM runs WHERE started_at < ? AND run_id != ?")
        .bind(&cutoff)
        .bind(&run.run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM runs WHERE run_id NOT IN \
         (SELECT run_id FROM runs ORDER BY started_at DESC LIMIT ?)",
    )
    .bind(run.retention.max_runs.max(1) as i64)
    .execute(&mut *tx)
    .await?;
    for table in ["step_runs", "log", "metrics", "metric_samples"] {
        // Safe: the four names are the literals above, never input.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE run_id NOT IN (SELECT run_id FROM runs)"
        )))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

async fn end_run(pool: &SqlitePool, run_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE runs SET finished_at = ? WHERE run_id = ?")
        .bind(now_rfc3339())
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn flush(
    pool: &SqlitePool,
    run_id: &str,
    batch: Pending,
    series: &mut HashMap<(String, String, String), SeriesState>,
    last: bool,
) -> Result<(), sqlx::Error> {
    let empty = batch.steps.is_empty() && batch.logs.is_empty() && batch.metrics.is_empty();
    if empty && !last {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for s in batch.steps.into_values() {
        sqlx::query(
            "INSERT INTO step_runs \
               (run_id, step, state, attempt, started_at, finished_at, error, msg, updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(run_id, step) DO UPDATE SET \
               state=excluded.state, attempt=excluded.attempt, \
               started_at=COALESCE(excluded.started_at, step_runs.started_at), \
               finished_at=excluded.finished_at, error=excluded.error, \
               msg=excluded.msg, updated_at=excluded.updated_at",
        )
        .bind(run_id)
        .bind(&s.step)
        .bind(&s.state)
        .bind(s.attempt as i64)
        .bind(&s.started_at)
        .bind(&s.finished_at)
        .bind(&s.error)
        .bind(&s.msg)
        .bind(&s.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for l in &batch.logs {
        sqlx::query(
            "INSERT INTO log (run_id, step, attempt, ts, stream, level, target, thread, msg, fields) \
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(run_id)
        .bind(&l.step)
        .bind(l.attempt as i64)
        .bind(&l.ts)
        .bind(&l.stream)
        .bind(&l.level)
        .bind(&l.target)
        .bind(&l.thread)
        .bind(&l.msg)
        .bind(&l.fields)
        .execute(&mut *tx)
        .await?;
    }
    let now = Instant::now();
    for (key, m) in batch.metrics {
        sqlx::query(
            "INSERT INTO metrics (run_id, step, name, labels, value, updated_at) \
             VALUES (?,?,?,?,?,?) \
             ON CONFLICT(run_id, step, name, labels) DO UPDATE SET \
               value=excluded.value, updated_at=excluded.updated_at",
        )
        .bind(run_id)
        .bind(&m.step)
        .bind(&m.name)
        .bind(&m.labels)
        .bind(m.value)
        .bind(&m.updated_at)
        .execute(&mut *tx)
        .await?;
        let due = match series.get(&key) {
            None => true,
            Some(s) => s.last_sample_value != m.value && now - s.last_sample_at >= SAMPLE_EVERY,
        };
        if due {
            insert_sample(&mut tx, run_id, &m).await?;
            series.insert(
                key,
                SeriesState {
                    last_sample_at: now,
                    last_sample_value: m.value,
                    current: m,
                },
            );
        } else if let Some(s) = series.get_mut(&key) {
            s.current = m;
        }
    }
    if last {
        // The final value of every series is a sample, however recent
        // the previous one: a rate drawn to the end of the run needs it.
        for s in series.values_mut() {
            if s.current.value != s.last_sample_value {
                insert_sample(&mut tx, run_id, &s.current).await?;
                s.last_sample_value = s.current.value;
            }
        }
    }
    tx.commit().await
}

async fn insert_sample(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    m: &MetricRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO metric_samples (run_id, step, name, labels, ts, value) VALUES (?,?,?,?,?,?)",
    )
    .bind(run_id)
    .bind(&m.step)
    .bind(&m.name)
    .bind(&m.labels)
    .bind(&m.updated_at)
    .bind(m.value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
