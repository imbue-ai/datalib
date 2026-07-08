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
//! - qmd setup failure is a warning dialog + degraded search
//!   (`qmd_daemon: None` falls back to the per-call shell-out path),
//!   not a hard startup failure — a desktop app that refuses to open
//!   is worse than one with slower search.
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
use tauri::{AppHandle, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

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
            prompt_for_data_root(app.handle().clone());
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

/// Show the folder picker. Picking a folder boots the backend and opens
/// the main window; canceling exits the app (there is nothing to show
/// without a data root).
fn prompt_for_data_root(app: AppHandle) {
    let mut picker = app
        .dialog()
        .file()
        .set_title("Select your Frankweiler data root");
    if let Some(dir) = default_picker_dir() {
        picker = picker.set_directory(dir);
    }
    picker.pick_folder(move |choice| match choice {
        Some(file_path) => match file_path.into_path() {
            Ok(root) => {
                tauri::async_runtime::spawn(boot(app, root));
            }
            Err(e) => fatal(&app, format!("unusable folder selection: {e}")),
        },
        None => app.exit(0),
    });
}

/// Seed the picker with the data root named in
/// `~/.config/frankweiler/config.yaml` when that file exists and the
/// root is on disk; otherwise let the OS pick its default location.
fn default_picker_dir() -> Option<PathBuf> {
    let cfg = frankweiler_ingest_config::load_config(None).ok()?;
    cfg.data_root.is_dir().then_some(cfg.data_root)
}

async fn boot(app: AppHandle, root: PathBuf) {
    let (url, qmd_warning) = match start_backend(root).await {
        Ok(started) => started,
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
    if let Some(warning) = qmd_warning {
        app.dialog()
            .message(format!(
                "qmd could not be started — search falls back to a slower, \
                 less relevant path.\n\n{warning}"
            ))
            .title("Frankweiler: degraded search")
            .kind(MessageDialogKind::Warning)
            .show(|_| {});
    }
}

/// Open the data root and serve the embedded UI + `/api/*` on an
/// ephemeral localhost port. Returns the base URL and, when qmd could
/// not be started, the warning to surface to the user.
async fn start_backend(root: PathBuf) -> anyhow::Result<(String, Option<String>)> {
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    }
    let root = Arc::new(root);
    let db_path = frankweiler_core::layout::backend_index_db(&root);
    let repo: frankweiler_core::repo::DynRepo = Arc::new(
        DoltRepo::open(&db_path, root.clone())
            .await
            .map_err(|e| anyhow::anyhow!("open doltlite at {}: {e}", db_path.display()))?,
    );

    let (qmd_daemon, qmd_warning) = match QmdDaemon::new(QmdDaemonConfig::new((*root).clone())) {
        Ok(daemon) => (Some(Arc::new(daemon)), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };

    // Self-contained config + sync plumbing, mirroring the standalone
    // `frankweiler-http` binary: the Setup tab reads/writes
    // `<root>/config.yaml`, the worker drains the `sync_jobs` queue by
    // driving `frankweiler-sync` against it, and progress fans out to
    // `GET /api/sync/stream` subscribers.
    let config_path = Arc::new(frankweiler_ingest_config::root_config_path(&root));
    let (progress_tx, _) = tokio::sync::broadcast::channel(512);
    let worker_cfg = frankweiler_http::worker::WorkerConfig {
        root: root.clone(),
        config_path: (*config_path).clone(),
        sync_bin: frankweiler_http::worker::resolve_sync_bin(),
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
    Ok((url, qmd_warning))
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
