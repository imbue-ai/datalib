//! Close-to-real reproducer for the doltlite × sqlx-sqlite BUSY bug.
//!
//! Mirrors frankweiler-sync's per-source RawDb pattern as closely as
//! possible without provider-specific code or network. Each "source":
//!
//!   1. Opens a SqlitePool (max_connections=1) on its own .doltlite_db,
//!      runs the same SHARED_DDL the real RawDb runs (sync_runs, blobs,
//!      plus provider tables + bookkeeping).
//!   2. Inserts a sync_runs row via pool.begin()+execute+tx.commit (like
//!      doltlite_raw::start_run).
//!   3. Loops over N synthetic "items". For each:
//!        - SELECT to check if blob already exists (interleaved read).
//!        - pool.begin() + INSERT into provider table + INSERT into the
//!          bookkeeping sidecar + tx.commit (multi-statement upsert).
//!        - Every K items, sleep S ms to simulate HTTP latency between
//!          API calls — this creates the bursty pattern.
//!   4. UPDATEs the sync_runs row (finish_run).
//!   5. Runs SELECT dolt_commit('-Am', ?).
//!   6. Closes the pool.
//!
//! N sources run concurrently in tokio tasks. We count user-visible
//! BUSY errors on inserts and on the dolt_commit.

// Standalone debug/benchmark CLI. Doesn't run under frankweiler-sync,
// has no indicatif progress bars to corrupt, and emits its results +
// per-iteration progress directly to stderr. The workspace clippy.toml
// disallows raw eprintln!/println! because they'd race with the obs
// IndicatifWriter inside sync; that constraint doesn't apply here.
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

#[derive(Parser, Debug, Clone)]
#[command(about = "Close-to-real frankweiler-sync repro for doltlite BUSY")]
struct Args {
    #[arg(long, default_value_t = 9)]
    sources: usize,
    /// Items to upsert per source.
    #[arg(long, default_value_t = 200)]
    items: usize,
    /// Sleep this long after every `burst_size` items, to simulate the
    /// HTTP-await pattern in real provider fetches.
    #[arg(long, default_value_t = 20)]
    sleep_ms: u64,
    #[arg(long, default_value_t = 10)]
    burst_size: usize,
    /// Per-row JSON payload size in bytes.
    #[arg(long, default_value_t = 256)]
    row_size: usize,
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    keep: bool,

    /// Background TCP-socket churn. N tasks each open + close localhost
    /// sockets in a tight loop while the loadtest runs. Mimics the
    /// HTTPS-fetch fd activity in real sync_bin.
    #[arg(long, default_value_t = 0)]
    socket_churn: usize,

    /// Do a big "load all existing IDs" SELECT at startup (like Slack's
    /// `loaded_blob_ids` HashSet preload or ChatGPT's `conversation_states`).
    #[arg(long)]
    big_initial_scan: bool,

    /// Number of extra interleaved SELECT reads per item (in addition
    /// to the always-on blob_exists probe). Each does a different query
    /// pattern: COUNT(*), MAX(update_time), JOIN read, etc.
    #[arg(long, default_value_t = 0)]
    extra_reads_per_item: usize,

    /// Pre-open a second long-lived doltlite pool at <dir>/backend_index.doltlite_db
    /// and hold it for the entire run, mimicking sync_bin's backend_index
    /// (which is open from startup through translate + load phases).
    #[arg(long)]
    backend_index: bool,
}

#[derive(Default)]
struct Stats {
    sources: AtomicU64,
    upserts_ok: AtomicU64,
    upserts_busy: AtomicU64,
    commits_ok: AtomicU64,
    commits_busy: AtomicU64,
    commit_ms_total: AtomicU64,
    commit_ms_max: AtomicU64,
}

impl Stats {
    fn add_max(slot: &AtomicU64, v: u64) {
        let mut cur = slot.load(Ordering::Relaxed);
        while v > cur {
            match slot.compare_exchange_weak(cur, v, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(p) => cur = p,
            }
        }
    }
}

/// The exact DDL set the real RawDb runs at open.
const SHARED_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS sync_runs (
        run_id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at TEXT NOT NULL,
        finished_at TEXT NULL,
        config TEXT NOT NULL,
        status TEXT NOT NULL,
        summary TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS blobs (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        owning_id TEXT NOT NULL,
        slot TEXT NOT NULL,
        content_type TEXT NULL,
        sha256 TEXT NULL,
        bytes BLOB NULL,
        source_url TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS blobs_bookkeeping (
        id TEXT PRIMARY KEY,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS sync_scope_state (
        scope TEXT PRIMARY KEY,
        last_seen_at TEXT NOT NULL
    )",
    // Provider-specific data table (mimics chatgpt's `conversations`).
    "CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        title TEXT NULL,
        update_time TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS messages_bookkeeping (
        id TEXT PRIMARY KEY,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS messages_update ON messages(update_time)",
];

async fn open_pool(path: &std::path::Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .context("connect_with")?;
    for stmt in SHARED_DDL {
        sqlx::query(stmt)
            .execute(&pool)
            .await
            .with_context(|| format!("DDL: {}", &stmt[..40.min(stmt.len())]))?;
    }
    Ok(pool)
}

fn is_busy(e: &sqlx::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("locked") || s.contains("busy")
}

async fn run_one_source(idx: usize, dir: PathBuf, args: Args, stats: Arc<Stats>) -> Result<()> {
    let path = dir.join(format!("source_{idx:02}.doltlite_db"));
    let pool = open_pool(&path).await?;
    stats.sources.fetch_add(1, Ordering::Relaxed);

    // start_run equivalent
    {
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO sync_runs (started_at, config, status) VALUES (?, ?, 'running')")
            .bind("2026-06-05T00:00:00Z")
            .bind("{}")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }

    // Big initial scan — mimics Slack's `loaded_blob_ids` HashSet
    // preload and ChatGPT's `conversation_states` HashMap build.
    if args.big_initial_scan {
        let _: Vec<(String,)> = sqlx::query_as("SELECT id FROM messages")
            .fetch_all(&pool)
            .await
            .context("big_initial_scan messages")?;
        let _: Vec<(String,)> = sqlx::query_as("SELECT id FROM blobs WHERE bytes IS NOT NULL")
            .fetch_all(&pool)
            .await
            .context("big_initial_scan blobs")?;
    }

    let payload = format!(r#"{{"x":"{}"}}"#, "p".repeat(args.row_size.max(8) - 7));

    for i in 0..args.items {
        let id = format!("src{}_item{:06}", idx, i);

        // Interleaved read — like chatgpt's blob_exists / conversation_states
        let _: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM blobs WHERE id = ? AND bytes IS NOT NULL LIMIT 1")
                .bind(&id)
                .fetch_optional(&pool)
                .await
                .context("blob_exists probe")?;

        // Extra interleaved reads — varied query shapes so we exercise
        // multiple SQLite read code paths (full scan, indexed lookup,
        // aggregate, join).
        for k in 0..args.extra_reads_per_item {
            match k % 4 {
                0 => {
                    let _: (i64,) = sqlx::query_as("SELECT count(*) FROM messages")
                        .fetch_one(&pool)
                        .await
                        .context("count(*) messages")?;
                }
                1 => {
                    let _: Option<(String,)> =
                        sqlx::query_as("SELECT max(update_time) FROM messages")
                            .fetch_optional(&pool)
                            .await
                            .context("max update_time")?;
                }
                2 => {
                    let _: Option<(String, String)> = sqlx::query_as(
                        "SELECT m.id, mb.fetched_at
                         FROM messages m
                         LEFT JOIN messages_bookkeeping mb ON mb.id = m.id
                         WHERE m.id = ? LIMIT 1",
                    )
                    .bind(&id)
                    .fetch_optional(&pool)
                    .await
                    .context("join read")?;
                }
                _ => {
                    let _: Vec<(String,)> =
                        sqlx::query_as("SELECT id FROM messages WHERE id > ? ORDER BY id LIMIT 10")
                            .bind(&id)
                            .fetch_all(&pool)
                            .await
                            .context("range scan")?;
                }
            }
        }

        // Multi-statement upsert wrapped in an explicit tx (matches the
        // upsert_conversation_detail / upsert_blob pattern).
        let txr: Result<(), sqlx::Error> = async {
            let mut tx = pool.begin().await?;
            // Real RawDb upserts wrap the payload in jsonb(?) for binary
            // JSON storage. Use it here so we exercise the same SQL path.
            sqlx::query(
                "INSERT INTO messages (id, title, update_time, payload)
                 VALUES (?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title,
                    update_time = excluded.update_time,
                    payload = excluded.payload",
            )
            .bind(&id)
            .bind(format!("msg {i}"))
            .bind("2026-06-05T00:00:00Z")
            .bind(&payload)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO messages_bookkeeping (id, fetched_at, attempt_count)
                 VALUES (?, ?, 1)
                 ON CONFLICT(id) DO UPDATE SET
                    fetched_at = excluded.fetched_at,
                    attempt_count = messages_bookkeeping.attempt_count + 1",
            )
            .bind(&id)
            .bind("2026-06-05T00:00:00Z")
            .execute(&mut *tx)
            .await?;
            tx.commit().await
        }
        .await;

        match txr {
            Ok(()) => {
                stats.upserts_ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) if is_busy(&e) => {
                stats.upserts_busy.fetch_add(1, Ordering::Relaxed);
                eprintln!("[src {idx} item {i}] upsert BUSY: {e:#}");
            }
            Err(e) => {
                pool.close().await;
                return Err(anyhow::anyhow!("upsert {idx}/{i}: {e:#}"));
            }
        }

        // Burst-then-sleep — simulates HTTP-await pacing in real fetches.
        if args.sleep_ms > 0 && (i + 1) % args.burst_size == 0 && (i + 1) < args.items {
            tokio::time::sleep(Duration::from_millis(args.sleep_ms)).await;
        }
    }

    // finish_run equivalent
    {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "UPDATE sync_runs SET finished_at = ?, status = 'ok', summary = ? WHERE run_id = (SELECT MAX(run_id) FROM sync_runs)"
        )
        .bind("2026-06-05T00:00:30Z")
        .bind(r#"{"items":1}"#)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }

    // The headline event: SELECT dolt_commit('-Am', ?).
    let t = Instant::now();
    let res: Result<Option<String>, _> = sqlx::query_scalar("SELECT dolt_commit('-Am', ?)")
        .bind(format!("source {idx}: {} items committed", args.items))
        .fetch_optional(&pool)
        .await;
    let ms = t.elapsed().as_millis() as u64;
    stats.commit_ms_total.fetch_add(ms, Ordering::Relaxed);
    Stats::add_max(&stats.commit_ms_max, ms);
    match res {
        Ok(_) => {
            stats.commits_ok.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) if is_busy(&e) => {
            stats.commits_busy.fetch_add(1, Ordering::Relaxed);
            eprintln!("[src {idx}] commit BUSY after {ms} ms: {e:#}");
        }
        Err(e) => {
            pool.close().await;
            return Err(anyhow::anyhow!("commit {idx}: {e:#}"));
        }
    }
    pool.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (dir, _guard): (PathBuf, Option<tempfile::TempDir>) = match args.dir.clone() {
        Some(p) => {
            std::fs::create_dir_all(&p)?;
            (p, None)
        }
        None => {
            let t = tempfile::tempdir()?;
            (t.path().to_path_buf(), Some(t))
        }
    };
    eprintln!(
        "loadtest: sources={} items_per_source={} burst_size={} sleep_ms={} dir={}",
        args.sources,
        args.items,
        args.burst_size,
        args.sleep_ms,
        dir.display()
    );

    // Pre-open the backend_index pool at the same scope as sync_bin
    // does — a second long-lived doltlite SqlitePool on a different
    // .doltlite_db file, held for the full run.
    let _backend_index_pool: Option<SqlitePool> = if args.backend_index {
        let p = dir.join("backend_index.doltlite_db");
        let pool = open_pool(&p).await?;
        eprintln!("backend_index pool open at {}", p.display());
        Some(pool)
    } else {
        None
    };

    let stats = Arc::new(Stats::default());
    let t0 = Instant::now();

    // Background TCP-socket churn — mimics the HTTPS-fetch fd activity
    // that real provider extracts generate. Each task opens a localhost
    // listener (which immediately fails to connect) so we cycle fds
    // without needing a real server.
    let churn_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut churn_handles = Vec::new();
    for _ in 0..args.socket_churn {
        let stop = churn_stop.clone();
        churn_handles.push(tokio::spawn(async move {
            use tokio::net::TcpStream;
            while !stop.load(Ordering::Relaxed) {
                // Try connecting to a closed port — fast failure that
                // cycles a socket fd.
                let _ = tokio::time::timeout(
                    Duration::from_millis(5),
                    TcpStream::connect("127.0.0.1:1"),
                )
                .await;
                tokio::task::yield_now().await;
            }
        }));
    }

    let mut set = tokio::task::JoinSet::new();
    for i in 0..args.sources {
        let dir = dir.clone();
        let args = args.clone();
        let stats = stats.clone();
        set.spawn(async move { run_one_source(i, dir, args, stats).await });
    }
    while let Some(joined) = set.join_next().await {
        joined??;
    }
    let wall = t0.elapsed();
    churn_stop.store(true, Ordering::Relaxed);
    for h in churn_handles {
        let _ = h.await;
    }

    let ok = stats.commits_ok.load(Ordering::Relaxed).max(1);
    eprintln!();
    eprintln!("=== loadtest results ===");
    eprintln!("wall_clock:      {:>8.3} s", wall.as_secs_f64());
    eprintln!("sources:         {}", stats.sources.load(Ordering::Relaxed));
    eprintln!(
        "upserts ok:      {}",
        stats.upserts_ok.load(Ordering::Relaxed)
    );
    eprintln!(
        "upserts BUSY:    {}",
        stats.upserts_busy.load(Ordering::Relaxed)
    );
    eprintln!(
        "commits ok:      {}",
        stats.commits_ok.load(Ordering::Relaxed)
    );
    eprintln!(
        "commits BUSY:    {}",
        stats.commits_busy.load(Ordering::Relaxed)
    );
    eprintln!(
        "avg commit ms:   {:>8.2}",
        stats.commit_ms_total.load(Ordering::Relaxed) as f64 / ok as f64
    );
    eprintln!(
        "max commit ms:   {}",
        stats.commit_ms_max.load(Ordering::Relaxed)
    );
    if args.keep {
        eprintln!("(--keep): {}", dir.display());
    }
    Ok(())
}
