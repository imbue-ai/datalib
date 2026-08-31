//! Writing the bus, and reading it.
//!
//! # Why the writer is a thread and a channel
//!
//! The runner publishes through `EventSink::emit(&self, …)`, which is
//! synchronous and called from wherever the event arose — the scheduler
//! task, or the reader thread draining a step subprocess's stdout. A
//! sink that blocked on a database write there would put I/O on the path
//! of every progress tick a step emits, which for a chatty download is
//! thousands.
//!
//! So [`ProgressWriter::update`] only puts the newest state for a step
//! into a map and returns. A background thread owns the connection and
//! flushes on an interval.
//!
//! That thread is joined by `Drop`, which makes the writer's own scope
//! the bound on its lifetime — the nursery discipline from Nathaniel J.
//! Smith's "Notes on structured concurrency, or: Go statement
//! considered harmful" (<https://vorpus.org/blog/notes-on-structured-concurrency-or-go-statement-considered-harmful/>).
//! A caller that leaves by a path running no destructors — a bare
//! `std::process::exit` — defeats it and gets an empty bus, so such a
//! caller must give the writer a scope that closes first. `datalib-dag`
//! does exactly that, and `//tests/fixtures:progress_bus_e2e_test`
//! holds it to it.
//!
//! # Coalescing is correctness, not a shortcut
//!
//! Only the newest tick per step survives a flush. That is what
//! progress *is*: "347 of 900" supersedes "346 of 900" completely, and a
//! reader who polls twice a second wants the newest, not a backlog of
//! 400. Terminal states are the exception — they must never be
//! overwritten by a straggling tick that was already in flight, so
//! [`ProgressWriter::finish`] latches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::{progress_path, ProgressRow, SCHEMA};

/// How often the writer thread flushes. 200ms is under the threshold
/// where a progress bar reads as laggy, and far above the cost of the
/// write (~0.3ms per row on a plain-SQLite file, measured).
const FLUSH_EVERY: Duration = Duration::from_millis(200);

/// Build the connect string for a bus at `path`.
///
/// Two things are load-bearing here.
///
/// **`doltlite_engine=sqlite`** is doltlite's own opt-out: it selects
/// the stock engine for a *new empty* file, and is ignored once the file
/// has content (`sqlite3BtreeOpen` in the amalgamation). Without it,
/// creating this file yields a `CTLD` prolly-tree store, whose ~50ms
/// per committed write and `.<name>-lock` sidecar are exactly what a
/// progress bar cannot afford.
///
/// **`file:` prefix.** doltlite reads the parameter with
/// `sqlite3_uri_parameter`, which only sees a URI-shaped name — a plain
/// path is under-read and the parameter silently does nothing. sqlx
/// always passes `SQLITE_OPEN_URI` and hands the filename to
/// `sqlite3_open_v2` **verbatim** *provided it has no URI parameters of
/// its own to add*; if it does (`immutable`, `vfs`), it percent-encodes
/// the whole filename and our parameter becomes part of the path. So do
/// not set either of those on these options. `open_is_really_stock_sqlite`
/// is the test that fails if this ever stops holding.
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
        // Nothing here survives the next run by design, so there is
        // nothing worth an fsync. A torn bar after a power cut costs
        // exactly nothing.
        .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
}

/// Open the bus at `path`, creating it as a plain-SQLite file if absent.
pub async fn open_or_create(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(sqlx::Error::Io)?;
    }
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options(path, true))
        .await
}

/// Open an existing bus read-only-ish. Never creates: a reader that
/// created the file would race the runner for which engine claims it.
async fn open_existing(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options(path, false))
        .await
}

/// The whole bus, as a reader sees it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    /// Which run these rows describe. `None` for an empty or absent
    /// bus. A reader comparing this against the run it is displaying is
    /// how it avoids painting one run's bars onto another: the bus is
    /// remade at the start of every run, so between a run starting and
    /// the runner reaching its first step, the rows still on disk
    /// belong to the run before.
    pub run_id: Option<String>,
    pub steps: Vec<ProgressRow>,
}

/// Everything a reader needs, in one query.
pub async fn snapshot(data_root: &Path) -> Snapshot {
    let path = progress_path(data_root);
    if !path.exists() {
        return Snapshot::default();
    }
    let Ok(pool) = open_existing(&path).await else {
        return Snapshot::default();
    };
    let rows = sqlx::query(
        "SELECT step, run_id, state, done, total, msg, updated_at \
         FROM step_progress ORDER BY step",
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;
    Snapshot {
        run_id: rows.first().map(|r| r.get("run_id")),
        steps: rows
            .iter()
            .map(|r| ProgressRow {
                step: r.get("step"),
                state: r.get("state"),
                done: r.get("done"),
                total: r.get("total"),
                msg: r.get("msg"),
                updated_at: r.get("updated_at"),
            })
            .collect(),
    }
}

type Pending = Arc<Mutex<BTreeMap<String, ProgressRow>>>;

/// Publishes step state to the bus. Cheap to call; the work happens on
/// its own thread.
pub struct ProgressWriter {
    pending: Pending,
    /// Dropping this tells the writer thread to flush once more and
    /// exit, which is what makes a run's final states land. It must be
    /// dropped *before* joining — joining while still holding the
    /// sender waits forever on a thread that has not been told to stop.
    ///
    /// Behind a `Mutex` rather than owned, so [`Self::finish`] works
    /// through `&self`: the sink that holds this lives inside an `Arc`,
    /// and `datalib-dag` ends in `std::process::exit`, which runs no
    /// destructors at all.
    stop: Mutex<Option<mpsc::Sender<()>>>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ProgressWriter {
    /// Start a fresh bus at `data_root` and write to it.
    ///
    /// The file is discarded and remade every run: this is live state,
    /// so last run's rows are noise, and remaking it also repairs a path
    /// that somehow ended up holding a doltlite-format file — which,
    /// having content, would otherwise stay `CTLD` forever.
    ///
    /// Returns `None` — rather than failing the run — when the bus
    /// cannot be opened. Progress is a convenience; a sync that works
    /// but cannot draw a bar is far better than one that refuses to
    /// start because a status file was unwritable.
    pub fn start(data_root: &Path, run_id: &str) -> Option<Self> {
        let path = progress_path(data_root);
        // Sidecars describe a database that is about to stop existing;
        // leaving one beside a fresh file is how you get "database disk
        // image is malformed".
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = path.as_os_str().to_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        let pending: Pending = Default::default();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("progress-bus".into())
            .spawn({
                let pending = pending.clone();
                let run_id = run_id.to_string();
                move || writer_loop(path, run_id, pending, rx)
            })
            .ok()?;
        Some(Self {
            pending,
            stop: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        })
    }

    /// Flush everything pending and stop the writer thread.
    ///
    /// `Drop` calls this, which is the intended path — but a caller
    /// that ends in `std::process::exit` runs no destructors at all,
    /// and would leave the bus holding whatever the last interval
    /// flush happened to catch. Such a caller should give the writer a
    /// scope that closes before it exits (`datalib-dag` does) rather
    /// than reach for this. Idempotent either way.
    fn finish(&self) {
        // Sender first — that disconnect is the loop's exit signal.
        drop(self.stop.lock().expect("progress bus stop mutex").take());
        let handle = self
            .handle
            .lock()
            .expect("progress bus handle mutex")
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    /// Record the newest state for a step, replacing whatever was
    /// pending for it.
    pub fn update(&self, next: ProgressRow) {
        let mut map = self.pending.lock().expect("progress bus mutex");
        // A terminal state latches: a tick that was already in flight
        // when the step finished must not resurrect it as running.
        if let Some(prev) = map.get(&next.step) {
            if is_terminal(&prev.state) && !is_terminal(&next.state) {
                return;
            }
        }
        map.insert(next.step.clone(), next);
    }
}

impl Drop for ProgressWriter {
    fn drop(&mut self) {
        // The ordinary path. `finish` is what guarantees the final
        // flush landed before the process reports the run as finished.
        self.finish();
    }
}

/// Anything that isn't pending or running is where a step stops.
fn is_terminal(state: &str) -> bool {
    !matches!(state, "pending" | "running")
}

fn writer_loop(path: PathBuf, run_id: String, pending: Pending, stop: mpsc::Receiver<()>) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let Ok(pool) = rt.block_on(open_or_create(&path)) else {
        tracing::warn!(path = %path.display(), "progress bus: open failed; no live progress");
        return;
    };
    if rt.block_on(sqlx::query(SCHEMA).execute(&pool)).is_err() {
        return;
    }

    loop {
        // Wake on the interval, or immediately when the writer is
        // dropped — whose disconnect is the signal to flush and go.
        let done = matches!(
            stop.recv_timeout(FLUSH_EVERY),
            Err(RecvTimeoutError::Disconnected) | Ok(())
        );
        let batch: Vec<ProgressRow> = {
            let mut map = pending.lock().expect("progress bus mutex");
            std::mem::take(&mut *map).into_values().collect()
        };
        if !batch.is_empty() {
            rt.block_on(flush(&pool, &run_id, &batch));
        }
        if done {
            break;
        }
    }
    rt.block_on(pool.close());
}

async fn flush(pool: &SqlitePool, run_id: &str, batch: &[ProgressRow]) {
    for p in batch {
        let res = sqlx::query(
            "INSERT INTO step_progress (step, run_id, state, done, total, msg, updated_at) \
             VALUES (?,?,?,?,?,?,?) \
             ON CONFLICT(step) DO UPDATE SET \
               run_id=excluded.run_id, state=excluded.state, done=excluded.done, \
               total=excluded.total, msg=excluded.msg, updated_at=excluded.updated_at",
        )
        .bind(&p.step)
        .bind(run_id)
        .bind(&p.state)
        .bind(p.done)
        .bind(p.total)
        .bind(&p.msg)
        .bind(&p.updated_at)
        .execute(pool)
        .await;
        if let Err(e) = res {
            // One bad row must not stop the bar for every other step,
            // and must never take the run down.
            tracing::warn!(error = %e, step = %p.step, "progress bus: write failed");
        }
    }
}
