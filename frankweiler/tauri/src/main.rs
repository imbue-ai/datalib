//! Frankweiler Tauri shell.
//!
//! A thin process manager around the real backend: on startup a native
//! folder picker asks for the data root (skipped when one is supplied
//! via CLI arg / `$FRANKWEILER_DATA_ROOT`), then the shell spawns the
//! bundled **`frankweiler-http` binary** — the exact same binary the
//! web packaging runs — as a child process on an ephemeral 127.0.0.1
//! port and opens the main window at its URL. That server serves both
//! the rust-embed'd Vue UI and `/api/*`, so the UI's relative
//! `fetch('/api/…')` transport works unchanged.
//!
//! The backend is deliberately NOT linked in-process: one binary, one
//! behavior. Everything backend-side (DB layout, config, qmd, sync
//! worker) is whatever `frankweiler-http` does — the shell only decides
//! *which* binary to run and passes one presentation flag (`--no-open`:
//! the window replaces the browser tab).
//!
//! Port handshake: the child gets `FRANKWEILER_BIND=127.0.0.1:0` and
//! `--url-file <tmp>`; it writes its bound URL there as soon as the
//! listener exists, and the shell polls for that file. No port
//! pre-allocation race, no log parsing.
//!
//! The child is killed when the app exits (see the `RunEvent::Exit`
//! handler in `main`).
//!
//! The `frankweiler://` deep-link handler is still TODO — see
//! blueprint/frankweiler-ui/plan-frankweiler-ui.md §F8.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// The spawned `frankweiler-http` child, managed in tauri state so the
/// exit handler can kill it. `None` until boot succeeds.
struct HttpChild(Mutex<Option<Child>>);

#[tauri::command]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() {
    #[cfg(target_os = "macos")]
    inherit_shell_path();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![version])
        .manage(HttpChild(Mutex::new(None)))
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
        .build(tauri::generate_context!())
        .expect("error while building frankweiler tauri app");

    app.run(|app, event| {
        // The backend child must not outlive the window: an orphaned
        // server would keep the doltlite file open and hold the port.
        if let tauri::RunEvent::Exit = event {
            let taken = app
                .state::<HttpChild>()
                .0
                .lock()
                .expect("http child lock")
                .take();
            if let Some(mut c) = taken {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    });
}

/// Apps launched from Finder/Dock inherit launchd's minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which lacks the Homebrew / nvm
/// directories where node and npx live — and the backend's qmd search
/// shells out to `npx`. Capture the user's login-shell PATH instead
/// (the spawned `frankweiler-http` child inherits it), the same trick
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

/// Locate the `frankweiler-http` binary to spawn. Dev override
/// `$FRANKWEILER_HTTP_BIN` wins (point it at a fresh Bazel build
/// without rebundling); otherwise the copy bundled under
/// `Contents/Resources/binaries/` (see `tauri.conf.json`
/// `bundle.resources`), which `resource_dir()` resolves regardless of
/// where the bundle lives. The sibling `frankweiler-sync` there is
/// found by the child's own sibling-of-executable lookup, so no sync
/// path needs to be threaded through.
fn resolve_http_bin(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FRANKWEILER_HTTP_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!("$FRANKWEILER_HTTP_BIN={} is not a file", p.display());
    }
    let p = app.path().resource_dir().ok()?.join("binaries/frankweiler-http");
    p.is_file().then_some(p)
}

async fn boot(app: AppHandle, root: PathBuf) {
    let url = match tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || start_backend(&app, root)
    })
    .await
    {
        Ok(Ok(url)) => url,
        Ok(Err(e)) => return fatal(&app, format!("could not start the backend: {e:#}")),
        Err(e) => return fatal(&app, format!("backend startup task panicked: {e}")),
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

/// Spawn the bundled `frankweiler-http` against `root` on an ephemeral
/// localhost port and wait (≤15s) for it to announce its URL via
/// `--url-file`. The child's output goes to a log file in the temp dir
/// so startup failures can quote it in the error dialog (a
/// Finder-launched app has no terminal). Blocking: run on a worker
/// thread, not the event loop.
fn start_backend(app: &AppHandle, root: PathBuf) -> anyhow::Result<String> {
    let http_bin = resolve_http_bin(app).ok_or_else(|| {
        anyhow::anyhow!(
            "frankweiler-http binary not found (no bundled copy and \
             $FRANKWEILER_HTTP_BIN not set)"
        )
    })?;

    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let url_file = tmp.join(format!("frankweiler-http-{pid}.url"));
    let log_file = tmp.join(format!("frankweiler-http-{pid}.log"));
    // Remove a stale url-file from a recycled PID so we can't read a
    // dead server's address.
    let _ = std::fs::remove_file(&url_file);

    let log = std::fs::File::create(&log_file)
        .map_err(|e| anyhow::anyhow!("create backend log {}: {e}", log_file.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| anyhow::anyhow!("clone backend log handle: {e}"))?;

    let mut child = Command::new(&http_bin)
        .arg(&root)
        .arg("--no-open")
        .arg("--url-file")
        .arg(&url_file)
        .env("FRANKWEILER_BIND", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", http_bin.display()))?;

    // Poll for the URL announcement, watching for an early child death
    // so a bad data root fails with the backend's own message instead
    // of a timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let url = loop {
        if let Ok(url) = std::fs::read_to_string(&url_file) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                break url;
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "frankweiler-http exited during startup ({status}):\n{}",
                log_tail(&log_file)
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "frankweiler-http did not announce its URL within 15s \
                 (log: {}):\n{}",
                log_file.display(),
                log_tail(&log_file)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = std::fs::remove_file(&url_file);

    *app.state::<HttpChild>().0.lock().expect("http child lock") = Some(child);
    Ok(url)
}

/// Last ~20 lines of the backend log, for error dialogs.
fn log_tail(path: &std::path::Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::from("(no backend log captured)");
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(20);
    lines[start..].join("\n")
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
