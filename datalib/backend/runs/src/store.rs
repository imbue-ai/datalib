//! Writing the store, and reading it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::{is_terminal, runs_path, Retention, INDEXES, SCHEMA_VERSION};
use app_schema::runs::{LogRow, MetricRow, MetricSampleRow, RunRow, StepRunRow};

/// How often the writer thread flushes. 200ms is under the threshold
/// where a progress display reads as laggy, and far above the cost of
/// the write (~0.3ms per row on a plain-SQLite file, measured).
const FLUSH_EVERY: Duration = Duration::from_millis(200);

/// How far back a snapshot looks for samples. A rate is a live
/// question, and the snapshot is read on every `dag_changed` frame —
/// several times a second during a run — so the window query must not
/// scan a day-long run's every sample each time.
const RATE_WINDOW: Duration = Duration::from_secs(10 * 60);

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
    for ddl in app_schema::runs::ddl()
        .into_iter()
        .chain(INDEXES.iter().copied())
    {
        sqlx::query(ddl).execute(pool).await?;
    }
    // Safe: a compile-time integer, not input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {SCHEMA_VERSION}"
    )))
    .execute(pool)
    .await?;
    Ok(())
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
    /// The two newest samples of every series — enough for a rate.
    /// Oldest first within a series.
    pub recent_samples: Vec<MetricSampleRow>,
}

/// The newest run.
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
    // The two newest per series, by a window over the run's recent
    // samples only. Text order is instant order, so `ts` sorts and the
    // cutoff is a plain comparison.
    let (cutoff, _) = datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(RATE_WINDOW.as_micros() as i64))
        .to_utc_and_offset();
    let recent_samples = sqlx::query(
        "SELECT step, name, labels, ts_utc, tz_offset, value FROM ( \
           SELECT *, ROW_NUMBER() OVER \
             (PARTITION BY step, name, labels ORDER BY ts_utc DESC) AS rn \
           FROM metric_samples WHERE run_id = ? AND ts_utc > ?) \
         WHERE rn <= 2 ORDER BY step, name, labels, ts_utc",
    )
    .bind(&run_id)
    .bind(&cutoff)
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
        recent_samples,
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
    log_where(data_root, Some(run_id), step, after_seq, limit).await
}

/// One step's lines across every run the store holds, oldest first —
/// "what has this step been doing", not "what did it do in this run".
/// Runs never overlap (the runner holds a lock), so `seq` order is also
/// run order, and the same tail cursor works across them.
pub async fn step_log_after(
    data_root: &Path,
    step: &str,
    after_seq: i64,
    limit: i64,
) -> Vec<LogRow> {
    log_where(data_root, None, Some(step), after_seq, limit).await
}

async fn log_where(
    data_root: &Path,
    run_id: Option<&str>,
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
        "SELECT seq, run_id, step, attempt, ts_utc, tz_offset, stream, level, target, thread, \
         msg, fields \
         FROM log WHERE (? IS NULL OR run_id = ?) AND seq > ? AND (? IS NULL OR step = ?) \
         ORDER BY seq LIMIT ?",
    )
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
    rows.iter()
        .map(|r| LogRow {
            seq: r.get("seq"),
            run_id: r.get("run_id"),
            step: r.get("step"),
            attempt: r.get("attempt"),
            ts_utc: r.get("ts_utc"),
            tz_offset: r.get("tz_offset"),
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
    steps: BTreeMap<String, StepRunRow>,
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
        started_at_utc: &str,
        retention: Retention,
    ) -> Option<Self> {
        let path = runs_path(data_root);
        let pending: Shared = Default::default();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("run-store".into())
            .spawn({
                let pending = pending.clone();
                // The start stamp arrives as the runner wrote it, offset
                // and all; the store keeps UTC and the offset apart.
                let (started_at_utc, tz_offset) = split_stamp(started_at_utc);
                let run = RunInfo {
                    run_id: run_id.to_string(),
                    started_at_utc,
                    tz_offset,
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

    /// The row's `run_id` is ignored: every row this writer takes belongs
    /// to the run it was started for, and it binds that.
    pub fn step(&self, next: StepRunRow) {
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
    started_at_utc: String,
    tz_offset: Option<String>,
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

/// Now, as the store keeps it.
pub fn now_split() -> (String, Option<String>) {
    let (utc, offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    (utc, Some(offset))
}

/// Record this run and apply retention. The run's own row goes in first
/// so the count limit includes it.
async fn begin_run(pool: &SqlitePool, run: &RunInfo) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runs (run_id, started_at_utc, tz_offset) VALUES (?, ?, ?) \
         ON CONFLICT(run_id) DO UPDATE SET started_at_utc = excluded.started_at_utc, \
           tz_offset = excluded.tz_offset, finished_at_utc = NULL",
    )
    .bind(&run.run_id)
    .bind(&run.started_at_utc)
    .bind(&run.tz_offset)
    .execute(&mut *tx)
    .await?;
    // Text order is instant order, now that every stamp is UTC.
    let (cutoff, _) = datalib_time::IsoOffsetTimestamp::now_local()
        .bump_micros(-(run.retention.max_age_days as i64) * 86_400 * 1_000_000)
        .to_utc_and_offset();
    sqlx::query("DELETE FROM runs WHERE started_at_utc < ? AND run_id != ?")
        .bind(&cutoff)
        .bind(&run.run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM runs WHERE run_id NOT IN \
         (SELECT run_id FROM runs ORDER BY started_at_utc DESC LIMIT ?)",
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
    let (finished_at_utc, _) = now_split();
    sqlx::query("UPDATE runs SET finished_at_utc = ? WHERE run_id = ?")
        .bind(finished_at_utc)
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
        sqlx::query(
            "INSERT INTO log (run_id, step, attempt, ts_utc, tz_offset, stream, level, target, thread, \
                              msg, fields) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(run_id)
        .bind(&l.step)
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
    let now = Instant::now();
    for (key, m) in batch.metrics {
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
    let sample = MetricSampleRow {
        run_id: run_id.to_string(),
        step: m.step.clone(),
        name: m.name.clone(),
        labels: m.labels.clone(),
        ts_utc: m.updated_at_utc.clone(),
        tz_offset: m.tz_offset.clone(),
        value: m.value,
    };
    sqlx::query(
        "INSERT OR REPLACE INTO metric_samples (run_id, step, name, labels, ts_utc, tz_offset, value) \
         VALUES (?,?,?,?,?,?,?)",
    )
    .bind(&sample.run_id)
    .bind(&sample.step)
    .bind(&sample.name)
    .bind(&sample.labels)
    .bind(&sample.ts_utc)
    .bind(&sample.tz_offset)
    .bind(sample.value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
