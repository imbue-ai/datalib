//! `frankweiler-http` — single-binary search backend.
//!
//! Usage: `frankweiler-http <data_root>`. The data root is the directory
//! that `frankweiler-sync` writes into: it contains
//! `backend_index.doltlite_db` (the SQL store), the `media/` symlinked
//! attachments, and `accounts.json`. The directory is created on demand
//! — first-run users get an empty index that fills in once they run a
//! sync.
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
    eprintln!(
        "frankweiler-http listening on http://{}",
        listener.local_addr()?
    );

    let root = Arc::new(root);
    let repo = build_repo(root.clone()).await?;
    let qmd_daemon = match QmdDaemon::new(QmdDaemonConfig::new((*root).clone())) {
        Ok(d) => {
            eprintln!("qmd daemon: ready (lazy spawn on first search)");
            Some(Arc::new(d))
        }
        Err(e) => {
            eprintln!("qmd daemon: disabled ({e:#}); falling back to CLI per call");
            None
        }
    };
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
