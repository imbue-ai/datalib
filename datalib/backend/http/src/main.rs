// Standalone HTTP server binary — runs standalone, no
// MultiProgress / no indicatif bars in this process. Exempt from the
// workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! `datalib-http` — single-binary search backend.
//!
//! Usage: `datalib-http <data_root> [--no-open] [--url-file <path>]`.
//! The data root is the directory the pipeline (`datalib-dag`) writes
//! into: one directory per source stanza, `unified_index/` for the
//! search indexes, and `system/` for this server's own state — the
//! feedback and job stores and the `system/media/` symlinked
//! attachments. The directory is created on demand.
//!
//! On startup we open the default browser at the listening URL so the
//! user doesn't need to copy-paste it; `--no-open` skips that, useful
//! for headless runs (CI, e2e tests, debugging) and for the Tauri
//! shell, which runs this binary as a child process (with
//! `--url-file <path>`; see the Args docs) and points its window at the
//! announced URL.
//!
//! Authentication: every request needs the per-process API token (see
//! [`datalib_http::auth`]). It rides in the announced URL as
//! `?token=…`, which the browser trades for a session cookie on the
//! first load; scripts and agents read it from
//! `<data_root>/system/api-token` and send
//! `Authorization: Bearer …`. Set `$DATALIB_TOKEN` to pin it (that's
//! how `dev.sh` shares one token with the Vite proxy).
//!
//! Bind address: `$DATALIB_BIND` if set, else `127.0.0.1:8731`. The
//! env override exists for the playwright e2e suite which needs an
//! ephemeral port per run; users running the bundled release just get
//! the default.
//!
//! Stores: `system/feedback.doltlite_db` and `system/jobs.doltlite_db`,
//! each through a one-connection `sqlx::SqlitePool`. The search indexes
//! belong to the `unified_index` applet and are never opened here.

use clap::Parser;
use datalib_http::{router, ApiToken};
use std::path::PathBuf;

const DEFAULT_BIND: &str = "127.0.0.1:8731";

#[derive(Debug, Parser)]
#[command(
    name = "datalib-http",
    about = "Single-binary search backend for the datalib data root.",
    long_about = None,
)]
struct Args {
    /// Data root directory written by the pipeline. Created if
    /// absent; an empty root produces an empty search index.
    data_root: PathBuf,

    /// Skip opening the default browser at the listening URL. Default
    /// is to open; pass this for headless / scripted runs (e2e tests,
    /// dev iteration where the tab is already open, CI).
    #[arg(long)]
    no_open: bool,

    /// After binding, write the base URL (e.g. `http://127.0.0.1:53829`)
    /// to this file. With `DATALIB_BIND=127.0.0.1:0` this is the
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
    let bind = std::env::var("DATALIB_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());

    // `build_state` creates the root when absent; the log line here just
    // makes the first-run case visible.
    if root.exists() {
        eprintln!("data root: {}", root.display());
    } else {
        eprintln!("data root: {} (created)", root.display());
    }

    // Claim the root before anything under `system/` is written —
    // including the token below, which a refused server would
    // otherwise clobber on its way out. Held for the life of the
    // process; the kernel releases it if we die.
    let mut root_lock =
        datalib_http::lock::DataRootLock::acquire(&root).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Minted before the announcement below because the announced URL
    // carries it: the browser (and the Tauri webview) authenticate by
    // loading `<url>?token=…` once, then ride the session cookie.
    let api_token = ApiToken::mint(&root)?;

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let url = format!("{base_url}/?token={}", api_token.value());
    // Record where we ended up, so a later would-be owner's refusal can
    // point at this server instead of just saying "taken".
    root_lock.announce(&base_url);
    eprintln!("datalib-http listening on {base_url}");
    eprintln!("open: {url}");
    eprintln!("api token: {}", api_token.token_file().display());

    // Announce the bound URL to a waiting parent process as soon as it
    // is known — before the (potentially slow) backend assembly below,
    // so the parent can point a webview/browser at it and let requests
    // queue in the listen backlog until `axum::serve` starts.
    if let Some(url_file) = &args.url_file {
        std::fs::write(url_file, &url)
            .map_err(|e| anyhow::anyhow!("write --url-file {}: {e}", url_file.display()))?;
        // The URL now carries the API token, and this file usually
        // lands in a shared /tmp. Same 0600 the token file gets.
        datalib_http::auth::restrict_to_owner(url_file)?;
    }

    if !args.no_open {
        // Best-effort browser open. We don't propagate the error
        // because most users will already have the tab from a prior
        // run (and `webbrowser::open` returns Ok in that case anyway).
        if let Err(e) = webbrowser::open(&url) {
            eprintln!("could not open browser at {url}: {e} (pass --no-open to silence)");
        }
    }

    // Everything root-derived (the feedback and job stores, the config
    // path, the sync worker) is assembled by the bootstrap shared with
    // the Tauri shell — see `datalib_http::boot`.
    let state = datalib_http::build_state(
        root,
        datalib_http::worker::resolve_dag_bin(),
        datalib_http::worker::resolve_binary_dir(),
        api_token,
    )
    .await?;

    eprintln!("config: {}", state.config_path().display());

    // Serve until a signal, then stop the applets on the way out.
    //
    // Without this the process had no signal handling at all, so a
    // SIGTERM landed on the default disposition and stopped it
    // mid-instruction: no unwind, no `Drop`, and therefore no
    // `Supervisor::drop` — the one thing that kills the applet
    // children. They were re-parented to init and ran until the
    // machine was rebooted. A laptop that had been running the app and
    // its test suite for a week was holding 186 of them (#238).
    //
    // `Drop` on the way out of `main` would not be enough on its own
    // either: `AppState` is cloned into the router and into the sync
    // worker, so the registry's refcount does not necessarily reach
    // zero here. Hence the explicit call.
    let applets = state.applets.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(terminated())
        .await?;
    eprintln!("datalib-http: shutting down, stopping applets");
    applets.shutdown();
    Ok(())
}

/// Resolve on the first signal that means "stop".
///
/// SIGKILL is deliberately absent because it cannot be caught — the
/// gateway can do nothing on that path, which is why the applet also
/// watches for its parent to disappear. See `applets.rs`.
async fn terminated() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // No handler is worse than a handler that never fires, but
            // both are survivable: the applet's own parent watch is the
            // backstop either way.
            Err(e) => {
                eprintln!("datalib-http: cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}
