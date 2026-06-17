// Standalone HTTP server binary — runs outside `frankweiler-sync`, no
// MultiProgress / no indicatif bars in this process. Exempt from the
// workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! `frankweiler-http` — single-binary search backend.
//!
//! Usage: `frankweiler-http <data_root> [--no-open]`. The data root is
//! the directory that `frankweiler-sync` writes into: it contains
//! `backend_index.doltlite_db` (the SQL store), the `media/` symlinked
//! attachments, and `accounts.json`. The directory is created on demand
//! — first-run users get an empty index that fills in once they run a
//! sync.
//!
//! On startup we open the default browser at the listening URL so the
//! user doesn't need to copy-paste it; `--no-open` skips that, useful
//! for headless runs (CI, e2e tests, debugging).
//!
//! Bind address: `$FRANKWEILER_BIND` if set, else `127.0.0.1:8731`. The
//! env override exists for the playwright e2e suite which needs an
//! ephemeral port per run; users running the bundled release just get
//! the default.
//!
//! Backend: [`DoltRepo`](frankweiler_core::dolt_repo::DoltRepo) over a
//! `sqlx::SqlitePool` against `<data_root>/backend_index.doltlite_db`.
//! No subprocess, no TCP port to MySQL.

use clap::Parser;
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::qmd::{qmd_cache_home, QmdDaemon, QmdDaemonConfig};
use frankweiler_core::repo::DynRepo;
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_BIND: &str = "127.0.0.1:8731";
const DOLT_DB_FILENAME: &str = "backend_index.doltlite_db";

#[derive(Debug, Parser)]
#[command(
    name = "frankweiler-http",
    about = "Single-binary search backend for the frankweiler data root.",
    long_about = None,
)]
struct Args {
    /// Data root directory written by `frankweiler-sync`. Created if
    /// absent; an empty root produces an empty search index.
    data_root: PathBuf,

    /// Skip opening the default browser at the listening URL. Default
    /// is to open; pass this for headless / scripted runs (e2e tests,
    /// dev iteration where the tab is already open, CI).
    #[arg(long)]
    no_open: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args.data_root;
    let bind = std::env::var("FRANKWEILER_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());

    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data_root {}: {e}", root.display()))?;
        eprintln!("data root: {} (created)", root.display());
    } else {
        eprintln!("data root: {}", root.display());
    }

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let url = format!("http://{}", listener.local_addr()?);
    eprintln!("frankweiler-http listening on {url}");

    if !args.no_open {
        // Best-effort browser open. We don't propagate the error
        // because most users will already have the tab from a prior
        // run (and `webbrowser::open` returns Ok in that case anyway).
        if let Err(e) = webbrowser::open(&url) {
            eprintln!("could not open browser at {url}: {e} (pass --no-open to silence)");
        }
    }

    let root = Arc::new(root);
    let repo = build_repo(root.clone()).await?;

    // Search runs on the qmd index. On a brand-new / empty data root the
    // index doesn't exist yet — that's the self-contained bootstrap case
    // (the whole point of "start from an empty root and sync from the
    // UI"), not an error. So we start *without* the daemon and let search
    // fall back (LIKE / per-call CLI) until the first sync builds the
    // index. A non-empty root with a genuinely *broken* qmd is still a
    // hard fail: a silent fallback there masks a bad install behind a
    // dramatically worse search the user may not notice.
    let index_path = frankweiler_core::qmd::qmd_index_path(&root);
    let qmd_daemon = if index_path.exists() {
        let daemon = QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))
            .map_err(|e| anyhow::anyhow!("qmd daemon: cannot start ({e:#})"))?;
        let daemon = Arc::new(daemon);
        // `qmd pull` ensures embedding + query-expansion + reranker models
        // are on disk before the first user query, so we don't pay a
        // multi-hundred-MB huggingface download on the interactive path.
        // Cache-checked, so a re-run on a warm box is free.
        eprintln!("qmd: pulling models…");
        let pull_cfg = daemon.config().clone();
        match tokio::task::spawn_blocking(move || run_qmd_pull(&pull_cfg)).await {
            Ok(Ok(())) => eprintln!("qmd: models ready"),
            Ok(Err(e)) => return Err(anyhow::anyhow!("qmd: pull failed ({e:#})")),
            Err(e) => return Err(anyhow::anyhow!("qmd: pull task panicked ({e})")),
        }
        Some(daemon)
    } else {
        eprintln!(
            "qmd: no index at {} yet — starting in search-fallback mode. \
             Set up a config + run a sync from the UI; restart for full \
             vector/hybrid search once the index is built.",
            index_path.display()
        );
        None
    };

    // Self-contained config: the app reads/writes `<root>/config.yaml`,
    // so a fresh data root needs no external `~/.config` file. The Setup
    // tab creates it; the worker drives `frankweiler-sync` against it.
    let config_path = Arc::new(frankweiler_core::config::root_config_path(&root));
    eprintln!("config: {}", config_path.display());

    // Live progress fan-out: the worker + enqueue/cancel handlers publish
    // here, `GET /api/sync/stream` subscribes. Buffer a few hundred events
    // so a briefly-stalled client lags rather than blocks the worker.
    let (progress_tx, _) = tokio::sync::broadcast::channel(512);

    // Background sync worker: drains the `sync_jobs` queue the UI fills.
    // Resolve the `frankweiler-sync` binary up front so the startup log
    // makes it obvious whether UI-triggered syncs will actually run.
    let sync_bin = frankweiler_http::worker::resolve_sync_bin();
    let worker_cfg = frankweiler_http::worker::WorkerConfig {
        root: root.clone(),
        config_path: (*config_path).clone(),
        sync_bin,
        progress_tx: progress_tx.clone(),
    };
    let worker_repo = repo.clone();
    tokio::spawn(async move {
        frankweiler_http::worker::run(worker_repo, worker_cfg).await;
    });

    let state = AppState {
        root,
        config_path,
        repo,
        qmd_daemon,
        progress_tx,
    };
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Shell out to `npx -y @tobilu/qmd@<ver> pull` with the daemon's
/// cache-home env, so any models qmd needs land in the same XDG cache
/// the daemon itself spawns against later.
fn run_qmd_pull(cfg: &QmdDaemonConfig) -> anyhow::Result<()> {
    let pkg = format!("@tobilu/qmd@{}", cfg.qmd_version);
    let status = std::process::Command::new("npx")
        .arg("-y")
        .arg(&pkg)
        .arg("pull")
        .env("XDG_CACHE_HOME", qmd_cache_home(&cfg.qmd_root))
        .status()
        .map_err(|e| anyhow::anyhow!("spawn npx (is Node.js installed?): {e}"))?;
    if !status.success() {
        anyhow::bail!("qmd pull exited with {status}");
    }
    Ok(())
}

async fn build_repo(root: Arc<PathBuf>) -> anyhow::Result<DynRepo> {
    let db_path = root.join(DOLT_DB_FILENAME);
    eprintln!("dolt db: {}", db_path.display());
    let repo = DoltRepo::open(&db_path, root)
        .await
        .map_err(|e| anyhow::anyhow!("open doltlite at {}: {e}", db_path.display()))?;
    Ok(Arc::new(repo))
}
