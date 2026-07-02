//! Frankweiler Tauri shell.
//!
//! Startup flow: no window at launch — a native folder picker asks for
//! the data root, then the axum backend from `frankweiler-http` (which
//! also serves the embedded Vue UI) is booted in-process on an
//! ephemeral 127.0.0.1 port, and the main window opens at that URL.
//! The UI's relative `fetch('/api/…')` calls resolve against the
//! embedded server's origin, so the web and Tauri packagings share the
//! whole transport layer (see the header comment in
//! `frankweiler/ui/src/api.ts`).
//!
//! Divergences from the standalone `frankweiler-http` binary:
//! - No qmd validation at startup: the daemon resolves its index lazily
//!   per search, so an empty root (no index yet) or a mid-session
//!   rebuild is handled transparently — search falls back until the
//!   index exists, then upgrades to qmd with no restart and no dialog.
//! - No `qmd pull` at startup: a multi-hundred-MB model download with
//!   no progress UI reads as a hung app. Models are pulled lazily by
//!   qmd itself on the first search that needs them.
//!
//! The `frankweiler://` deep-link handler is still TODO — see
//! blueprint/frankweiler-ui/plan-frankweiler-ui.md §F8.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::qmd::{QmdDaemon, QmdDaemonConfig};
use frankweiler_http::AppState;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// Same filename convention as `frankweiler-http`'s main.rs and
/// `frankweiler-sync` — the doltlite store inside the data root.
const DOLT_DB_FILENAME: &str = "backend_index.doltlite_db";

#[tauri::command]
fn version() -> &'static str {
    frankweiler_tauri_backend::version()
}

fn main() {
    #[cfg(target_os = "macos")]
    inherit_shell_path();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![version])
        .setup(|app| {
            let handle = app.handle().clone();
            // A data root supplied non-interactively (positional arg or
            // `$FRANKWEILER_DATA_ROOT`) skips the picker and boots
            // straight into it — mirrors `frankweiler_http_bin <root>`
            // and makes the app scriptable/testable. Otherwise fall back
            // to the native folder picker.
            match explicit_data_root() {
                Some(root) => {
                    tauri::async_runtime::spawn(boot(handle, root));
                }
                None => prompt_for_data_root(handle),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running frankweiler tauri app");
}

/// Apps launched from Finder/Dock inherit launchd's minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which lacks the Homebrew / nvm
/// directories where node and npx live — and qmd search shells out to
/// `npx`. Capture the user's login-shell PATH instead, the same trick
/// as the `fix-path-env` crate, without the extra dependency.
#[cfg(target_os = "macos")]
fn inherit_shell_path() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let Ok(out) = std::process::Command::new(&shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    if let Ok(path) = String::from_utf8(out.stdout) {
        let path = path.trim();
        if !path.is_empty() {
            std::env::set_var("PATH", path);
        }
    }
}

/// A data root supplied without the picker: first positional CLI arg,
/// else `$FRANKWEILER_DATA_ROOT`. A leading `~` is expanded against
/// `$HOME` (same convention as `dev.sh`), since `open --env` and shell
/// exports don't do tilde expansion. Returns `None` when neither is set,
/// leaving the interactive picker as the default.
fn explicit_data_root() -> Option<PathBuf> {
    let raw = std::env::args()
        .nth(1)
        .filter(|a| !a.is_empty())
        .or_else(|| std::env::var("FRANKWEILER_DATA_ROOT").ok())
        .filter(|a| !a.is_empty())?;
    let expanded = match raw.strip_prefix('~') {
        Some("") => std::env::var("HOME").unwrap_or(raw.clone()),
        Some(rest) if rest.starts_with('/') => {
            format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
        }
        _ => raw,
    };
    Some(PathBuf::from(expanded))
}

/// Show the folder picker. Picking a folder boots the backend and opens
/// the main window; canceling exits the app (there is nothing to show
/// without a data root).
fn prompt_for_data_root(app: AppHandle) {
    app.dialog()
        .file()
        .set_title("Select your Frankweiler data root")
        .pick_folder(move |choice| match choice {
            Some(file_path) => match file_path.into_path() {
                Ok(root) => {
                    tauri::async_runtime::spawn(boot(app, root));
                }
                Err(e) => fatal(&app, format!("unusable folder selection: {e}")),
            },
            None => app.exit(0),
        });
}

/// Locate the `frankweiler-sync` binary the in-process worker shells out
/// to. In a packaged `.app` it's bundled under `Contents/Resources/`
/// (see `tauri.conf.json` `bundle.resources`); `resource_dir()` resolves
/// that regardless of where the bundle lives. Returns `None` in a dev
/// `cargo run` (no bundle), where `resolve_sync_bin`'s
/// `$FRANKWEILER_SYNC_BIN` path takes over instead.
fn bundled_sync_bin(app: &AppHandle) -> Option<PathBuf> {
    let p = app.path().resource_dir().ok()?.join("binaries/frankweiler-sync");
    p.is_file().then_some(p)
}

async fn boot(app: AppHandle, root: PathBuf) {
    // Dev override ($FRANKWEILER_SYNC_BIN / a sibling binary) wins so a
    // fresh Bazel build can be pointed at without rebundling; otherwise
    // fall back to the copy bundled inside the .app.
    let sync_bin =
        frankweiler_http::worker::resolve_sync_bin().or_else(|| bundled_sync_bin(&app));
    let url = match start_backend(root, sync_bin).await {
        Ok(url) => url,
        Err(e) => return fatal(&app, format!("could not start the backend: {e:#}")),
    };
    let Ok(url) = url.parse() else {
        return fatal(&app, format!("backend produced an unusable URL: {url}"));
    };
    let window = WebviewWindowBuilder::new(&app, "main", WebviewUrl::External(url))
        .title("Frankweiler")
        .inner_size(1280.0, 800.0)
        .build();
    if let Err(e) = window {
        return fatal(&app, format!("could not open the main window: {e}"));
    }
}

/// Open the data root and serve the embedded UI + `/api/*` on an
/// ephemeral localhost port. Returns the base URL. `sync_bin` is the
/// `frankweiler-sync` path the sync worker shells out to (see
/// [`bundled_sync_bin`]); `None` leaves UI-triggered syncs to fail with
/// a clear message while search still works.
async fn start_backend(root: PathBuf, sync_bin: Option<PathBuf>) -> anyhow::Result<String> {
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    }
    let root = Arc::new(root);
    let db_path = root.join(DOLT_DB_FILENAME);
    let repo = DoltRepo::open(&db_path, root.clone())
        .await
        .map_err(|e| anyhow::anyhow!("open doltlite at {}: {e}", db_path.display()))?;
    // Coerce to the shared trait-object handle up front so the sync
    // worker and the router can each hold a clone.
    let repo: frankweiler_core::repo::DynRepo = Arc::new(repo);

    // The daemon is always present now: it resolves the index lazily on
    // each search, so a root whose qmd index is missing (empty root, sync
    // not run yet) or rebuilt mid-session is handled transparently —
    // search falls back until the index exists, then upgrades to qmd with
    // no restart. No index download here either (see the module header).
    let qmd_daemon = Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone())));

    // Self-contained config lives at `<root>/config.yaml` — same
    // convention as the standalone `frankweiler-http` binary, so the
    // Setup tab and the sync worker read/write the same file.
    let config_path = Arc::new(frankweiler_core::config::root_config_path(&root));

    // Live sync-job progress fan-out: the worker + enqueue/cancel
    // handlers publish here, `GET /api/sync/stream` subscribes over SSE.
    let (progress_tx, _) = tokio::sync::broadcast::channel(512);

    // Background sync worker: drains the `sync_jobs` queue the UI fills.
    // In a Bazel dev run `$FRANKWEILER_SYNC_BIN` points at the runfiles
    // binary; in a packaged app it's a sibling of this executable. When
    // neither is found the worker still runs — UI-triggered syncs fail
    // fast with a clear message instead of hanging (search is unaffected).
    let worker_cfg = frankweiler_http::worker::WorkerConfig {
        root: root.clone(),
        config_path: (*config_path).clone(),
        sync_bin,
        progress_tx: progress_tx.clone(),
    };
    let worker_repo = repo.clone();
    tauri::async_runtime::spawn(async move {
        frankweiler_http::worker::run(worker_repo, worker_cfg).await;
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let state = AppState {
        root,
        config_path,
        repo,
        qmd_daemon,
        progress_tx,
    };
    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, frankweiler_http::router(state)).await {
            eprintln!("embedded backend exited: {e}");
        }
    });
    Ok(url)
}

/// Surface a startup-fatal error in a dialog, then exit. `eprintln!` is
/// useless in a Finder-launched app — the dialog is the only channel
/// the user will actually see.
fn fatal(app: &AppHandle, msg: String) {
    eprintln!("{msg}");
    let handle = app.clone();
    app.dialog()
        .message(msg)
        .title("Frankweiler failed to start")
        .kind(MessageDialogKind::Error)
        .show(move |_| handle.exit(1));
}
