// Standalone HTTP server binary — runs outside `frankweiler-sync`, no
// MultiProgress / no indicatif bars in this process. Exempt from the
// workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! `frankweiler-http` — single-binary search backend.
//!
//! Usage: `frankweiler-http <data_root> [--no-open] [--url-file <path>]`.
//! The data root is the directory that `frankweiler-sync` writes into:
//! it contains one directory per source stanza plus `system/` holding
//! the SQL store (`system/backend_index/db.doltlite_db`), the
//! `system/media/` symlinked attachments, and the qmd index. The
//! directory is created on demand — first-run users get an empty index
//! that fills in once they run a sync.
//!
//! On startup we open the default browser at the listening URL so the
//! user doesn't need to copy-paste it; `--no-open` skips that, useful
//! for headless runs (CI, e2e tests, debugging) and for the Tauri
//! shell, which runs this binary as a child process (with
//! `--url-file <path>`; see the Args docs) and points its window at the
//! announced URL.
//!
//! Bind address: `$FRANKWEILER_BIND` if set, else `127.0.0.1:8731`. The
//! env override exists for the playwright e2e suite which needs an
//! ephemeral port per run; users running the bundled release just get
//! the default.
//!
//! Backend: [`DoltRepo`](frankweiler_core::dolt_repo::DoltRepo) over a
//! `sqlx::SqlitePool` against `<data_root>/system/backend_index/db.doltlite_db`.
//! No subprocess, no TCP port to MySQL.

use clap::Parser;
use frankweiler_http::router;
use std::path::PathBuf;

const DEFAULT_BIND: &str = "127.0.0.1:8731";

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

    /// After binding, write the base URL (e.g. `http://127.0.0.1:53829`)
    /// to this file. With `FRANKWEILER_BIND=127.0.0.1:0` this is the
    /// race-free way for a parent process (the Tauri shell, scripts) to
    /// learn the ephemeral port: poll for the file instead of parsing
    /// log output or pre-allocating a port.
    #[arg(long)]
    url_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args.data_root;
    let bind = std::env::var("FRANKWEILER_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());

    // `build_state` creates the root when absent; the log line here just
    // makes the first-run case visible.
    if root.exists() {
        eprintln!("data root: {}", root.display());
    } else {
        eprintln!("data root: {} (created)", root.display());
    }

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let url = format!("http://{}", listener.local_addr()?);
    eprintln!("frankweiler-http listening on {url}");

    // Announce the bound URL to a waiting parent process as soon as it
    // is known — before the (potentially slow) backend assembly below,
    // so the parent can point a webview/browser at it and let requests
    // queue in the listen backlog until `axum::serve` starts.
    if let Some(url_file) = &args.url_file {
        std::fs::write(url_file, &url)
            .map_err(|e| anyhow::anyhow!("write --url-file {}: {e}", url_file.display()))?;
    }

    if !args.no_open {
        // Best-effort browser open. We don't propagate the error
        // because most users will already have the tab from a prior
        // run (and `webbrowser::open` returns Ok in that case anyway).
        if let Err(e) = webbrowser::open(&url) {
            eprintln!("could not open browser at {url}: {e} (pass --no-open to silence)");
        }
    }

    // Everything root-derived (doltlite repo, qmd daemon, config path,
    // sync worker) is assembled by the bootstrap shared with the Tauri
    // shell — see `frankweiler_http::boot`.
    let state = frankweiler_http::build_state(
        root,
        frankweiler_http::worker::resolve_dag_bin(),
        frankweiler_http::worker::resolve_step_bin(),
    )
    .await?;
    let root = state.root.clone();

    // Search runs on the qmd index. The daemon resolves that index
    // lazily on each search, so it's always present — a brand-new/empty
    // root (no index yet) or a mid-run rebuild is handled transparently:
    // search falls back (LIKE / per-call CLI) until the index exists,
    // then upgrades to qmd with no restart.
    //
    // Models are lazy too — no `qmd pull` at boot. The indexer warms
    // the shared cache during every sync (embed pulls the embedding
    // model; its best-effort `qmd pull` grabs the rest), so any machine
    // that built its own index already has them. The remaining case —
    // index copied from elsewhere / wiped `~/.cache/qmd/models` — pays
    // a one-time download on the first semantic search instead of
    // blocking boot behind a multi-hundred-MB pull and turning a
    // network blip into a failed start.
    let index_path = frankweiler_core::qmd::qmd_index_path(&root);
    if index_path.exists() {
        // Models live once in a shared cache (`~/.cache/qmd/models`);
        // each data root reaches them through a `<root>/qmd/models`
        // symlink, so qmd — run with `XDG_CACHE_HOME=<root>` — resolves
        // lookups out to that one copy instead of re-downloading into
        // the root. The indexer creates this link during sync; ensure
        // it here too, so a backend booting a root the indexer hasn't
        // touched in this incarnation (or whose link went missing)
        // still shares the cache rather than silently pulling ~2 GB
        // into the data dir. Tolerate a pre-existing *real* dir rather
        // than hard-failing an existing install — we just won't share.
        let qmd_dir = frankweiler_core::layout::qmd_dir(&root);
        let models_dir = frankweiler_qmd_indexer::default_models_dir();
        if let Err(e) = std::fs::create_dir_all(&models_dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| frankweiler_qmd_indexer::ensure_models_symlink(&qmd_dir, &models_dir))
        {
            eprintln!(
                "qmd: could not ensure models symlink ({e:#}); \
                 continuing with {}/models as-is",
                qmd_dir.display()
            );
        }
        if !frankweiler_qmd_indexer::models_present(&qmd_dir.join("models")) {
            eprintln!(
                "qmd: model cache cold — the first semantic search will \
                 download models (one-time, shared across data roots)"
            );
        }
    } else {
        eprintln!(
            "qmd: no index at {} yet — search falls back until the first \
             sync builds it, then upgrades to qmd with no restart.",
            index_path.display()
        );
    }
    eprintln!("config: {}", state.config_path.display());

    axum::serve(listener, router(state)).await?;
    Ok(())
}
