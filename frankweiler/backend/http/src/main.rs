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
use frankweiler_core::qmd::{QmdDaemon, QmdDaemonConfig};
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
    // Startup is hard-fail on any qmd setup error: a silent fallback to
    // per-call CLI or LIKE-based search masks a broken install with a
    // dramatically worse experience that users may not notice. If qmd
    // can't run at all, the user should see that explicitly at startup
    // rather than discover it via "search is weirdly bad."
    let daemon = QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))
        .map_err(|e| anyhow::anyhow!("qmd daemon: cannot start ({e:#})"))?;
    let daemon = Arc::new(daemon);
    eprintln!("qmd daemon: warming up…");
    let warm = daemon.clone();
    match tokio::task::spawn_blocking(move || warm.warm_up()).await {
        Ok(Ok(())) => eprintln!("qmd daemon: ready"),
        Ok(Err(e)) => return Err(anyhow::anyhow!("qmd daemon: warmup failed ({e:#})")),
        Err(e) => return Err(anyhow::anyhow!("qmd daemon: warmup task panicked ({e})")),
    }
    let qmd_daemon = Some(daemon);
    let state = AppState {
        root,
        repo,
        qmd_daemon,
    };
    axum::serve(listener, router(state)).await?;
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
