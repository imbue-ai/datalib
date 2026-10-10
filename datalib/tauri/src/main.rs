//! Datalib Tauri shell.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod launcher;
mod raw_store;
mod zoom;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu};
use tauri::webview::{NewWindowFeatures, NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::StateFlags;

/// The spawned `datalib-http` child, managed in tauri state so the
/// exit handler can kill it. `None` until boot succeeds.
struct HttpChild(Mutex<Option<Child>>);

/// The data root the backend was started on; `None` until boot succeeds.
struct DataRoot(Mutex<Option<PathBuf>>);

/// The open library's server, as an origin (`http://127.0.0.1:<port>`):
/// what a window may navigate within. `None` on the libraries screen.
/// Serialized rather than kept as a `url::Origin`: that type is not
/// re-exported by tauri, and naming it would mean adding a direct `url`
/// dependency for one comparison.
struct AppOrigin(Mutex<Option<String>>);

/// The page zoom the View menu set (zoom.rs), applied to every window
/// and to each page as it loads.
struct PageZoom(Mutex<f64>);

const ZOOM_IN: &str = "zoom-in";
const ZOOM_OUT: &str = "zoom-out";
const ZOOM_ACTUAL: &str = "zoom-actual";

fn page_zoom(app: &AppHandle) -> f64 {
    *app.state::<PageZoom>().0.lock().unwrap()
}

fn zoom_file(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| zoom::zoom_file(&dir))
}

fn change_zoom(app: &AppHandle, step: impl Fn(f64) -> f64) {
    let state = app.state::<PageZoom>();
    let level = {
        let mut zoom = state.0.lock().unwrap();
        *zoom = step(*zoom);
        *zoom
    };
    for window in app.webview_windows().values() {
        let _ = window.set_zoom(level);
    }
    if let Some(file) = zoom_file(app) {
        if let Err(e) = zoom::save(&file, level) {
            eprintln!("could not keep the zoom in {}: {e}", file.display());
        }
    }
}

/// The app's menu: the platform's default, with Zoom In, Zoom Out and
/// Actual Size at the top of its View menu (one is added where the
/// default has none). Zoom In is ⌘=, the key ⌘+ is typed with, as in a
/// browser.
fn app_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::default(app)?;
    let zoom_in = MenuItem::with_id(app, ZOOM_IN, "Zoom In", true, Some("CmdOrCtrl+="))?;
    let zoom_out = MenuItem::with_id(app, ZOOM_OUT, "Zoom Out", true, Some("CmdOrCtrl+-"))?;
    let actual = MenuItem::with_id(app, ZOOM_ACTUAL, "Actual Size", true, Some("CmdOrCtrl+0"))?;
    let separator = PredefinedMenuItem::separator(app)?;
    let view = menu.items()?.into_iter().find_map(|item| match item {
        MenuItemKind::Submenu(sub) if sub.text().ok().as_deref() == Some("View") => Some(sub),
        _ => None,
    });
    match view {
        Some(view) => view.insert_items(&[&actual, &zoom_in, &zoom_out, &separator], 0)?,
        None => menu.append(&Submenu::with_items(
            app,
            "View",
            true,
            &[&actual, &zoom_in, &zoom_out],
        )?)?,
    }
    Ok(menu)
}

#[tauri::command]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// --- Launcher commands -----------------------------------------------------

#[tauri::command]
fn launcher_state(app: AppHandle) -> serde_json::Value {
    let dir = libraries_dir(&app);
    let home = home_dir(&app).unwrap_or_default();
    // TODO(after 2026-12-09): drop `legacy` and `move_from_documents`
    // with `launcher::move_from_documents`.
    let old_dir = documents_libraries_dir(&app);
    let legacy = old_dir.as_deref().map_or_else(Vec::new, |old| {
        launcher::libraries_in_documents(&launcher::recents_file(&home), old)
    });
    let libraries: Vec<serde_json::Value> =
        launcher::libraries(&launcher::recents_file(&home), &dir)
            .into_iter()
            .map(|l| {
                // A library in the libraries folder is known by its name; only
                // one somewhere else shows where it is.
                let elsewhere = l.path.parent() != Some(dir.as_path());
                serde_json::json!({
                    "name": launcher::display_name(&l.path),
                    "path": l.path.to_string_lossy(),
                    "shown_path": elsewhere.then(|| launcher::tilde(&l.path, &home)),
                    "forgettable": launcher::forgettable(&l.path, &dir),
                    "found": l.found,
                    "legacy": legacy.contains(&l.path),
                    "summary": launcher::summary(&l.path),
                })
            })
            .collect();
    let move_from = old_dir.filter(|_| !legacy.is_empty()).map(|old| {
        serde_json::json!({
            "count": legacy.len(),
            "from": launcher::tilde(&old, &home),
            "to": launcher::tilde(&dir, &home),
        })
    });
    serde_json::json!({
        "libraries": libraries,
        "libraries_dir": launcher::tilde(&dir, &home),
        "move_from_documents": move_from,
        "suggested_name": launcher::suggested_name(&dir),
    })
}

// TODO(after 2026-12-09): remove, with `launcher::move_from_documents`.
fn documents_libraries_dir(app: &AppHandle) -> Option<PathBuf> {
    let documents = app.path().document_dir().ok()?;
    Some(launcher::documents_libraries_dir(&documents))
}

/// Move the recent libraries in the Documents folder into the
/// libraries folder; how many moved. Only from the libraries screen,
/// where no library is open.
// TODO(after 2026-12-09): remove, with `launcher::move_from_documents`.
#[tauri::command]
fn launcher_move_from_documents(app: AppHandle) -> Result<usize, String> {
    if app
        .state::<DataRoot>()
        .0
        .lock()
        .expect("data root lock")
        .is_some()
    {
        return Err("Close the open library first.".into());
    }
    let home = home_dir(&app).ok_or("No home directory.")?;
    let old = documents_libraries_dir(&app).ok_or("No Documents folder.")?;
    launcher::move_from_documents(&launcher::recents_file(&home), &old, &libraries_dir(&app))
        .map(|moved| moved.len())
        .map_err(|e| format!("Could not move the libraries: {e}"))
}

/// What the new-library field names: the folder, as the popover shows
/// it, and what is there now. Null for an empty field.
#[tauri::command]
fn launcher_resolve(app: AppHandle, input: String) -> serde_json::Value {
    let home = home_dir(&app).unwrap_or_default();
    match launcher::resolve_new(&input, &home, &libraries_dir(&app)) {
        Some(root) => serde_json::json!({
            "shown_path": launcher::tilde(&root, &home),
            "path": root.to_string_lossy(),
            "target": launcher::classify(&root).as_str(),
        }),
        None => serde_json::Value::Null,
    }
}

/// Make the library the field names and open it on its Dashboard: the
/// starter config is written before the server starts (`--init`). A
/// folder that is already a library is opened instead.
#[tauri::command]
fn launcher_create(app: AppHandle, input: String) -> Result<(), String> {
    let home = home_dir(&app).unwrap_or_default();
    let root = launcher::resolve_new(&input, &home, &libraries_dir(&app))
        .ok_or("Type a name for the library, or a folder.")?;
    let shown = launcher::tilde(&root, &home);
    match launcher::classify(&root) {
        launcher::Target::Library => {
            tauri::async_runtime::spawn(boot(app, root, false));
        }
        launcher::Target::New => {
            std::fs::create_dir_all(&root).map_err(|e| format!("Could not create {shown}: {e}"))?;
            tauri::async_runtime::spawn(boot(app, root, true));
        }
        launcher::Target::Occupied => {
            return Err(format!(
                "{shown} has other files in it. Choose an empty folder, or a new name."
            ))
        }
        launcher::Target::NotAFolder => return Err(format!("{shown} is a file, not a folder.")),
    }
    Ok(())
}

/// The native folder picker, for the new-library field: the chosen
/// folder comes back as text for the field, and nothing is created
/// until Create.
#[tauri::command]
async fn launcher_choose_folder(app: AppHandle) -> Option<String> {
    let home = home_dir(&app).unwrap_or_default();
    app.dialog()
        .file()
        .set_title("Choose a folder for the new library")
        .blocking_pick_folder()
        .and_then(|choice| choice.into_path().ok())
        .map(|root| launcher::tilde(&root, &home))
}

/// Open a library the launcher listed. The path is checked rather than
/// trusted: the entry may have gone stale between render and click.
#[tauri::command]
fn launcher_open(app: AppHandle, path: String) -> Result<(), String> {
    let root = PathBuf::from(path);
    if !launcher::is_data_root(&root) {
        return Err(format!(
            "{} is no longer a data library — it may have been moved or deleted.",
            root.display()
        ));
    }
    tauri::async_runtime::spawn(boot(app, root, false));
    Ok(())
}

/// Open a library by picking its folder. Any folder is accepted: an
/// empty one gets the app's own first-run screen (see
/// `ui/src/views/FirstRunView.vue`). False when the picker was
/// canceled, so the page knows nothing is opening.
#[tauri::command]
async fn launcher_pick(app: AppHandle) -> Result<bool, String> {
    let Some(choice) = app
        .dialog()
        .file()
        .set_title("Open a library folder")
        .blocking_pick_folder()
    else {
        return Ok(false);
    };
    let root = choice
        .into_path()
        .map_err(|e| format!("unusable folder selection: {e}"))?;
    tauri::async_runtime::spawn(boot(app, root, false));
    Ok(true)
}

/// Take a library off the list; its folder is not touched. One in the
/// libraries folder is always listed while it is there, so it cannot be
/// forgotten.
#[tauri::command]
fn launcher_forget(app: AppHandle, path: String) -> Result<(), String> {
    if !launcher::forgettable(Path::new(&path), &libraries_dir(&app)) {
        return Err("A library in the Datalib folder is always listed.".into());
    }
    let home = home_dir(&app).ok_or("No home directory.")?;
    launcher::forget_recent(&launcher::recents_file(&home), Path::new(&path))
        .map_err(|e| e.to_string())
}

/// Open a library's folder in Finder (or the platform's file manager):
/// the folder itself, not its parent with it selected. Only a library:
/// the page names the path, and this must not open whatever it names.
#[tauri::command]
fn launcher_open_folder(app: AppHandle, path: String) -> Result<(), String> {
    let root = PathBuf::from(&path);
    if !launcher::is_data_root(&root) {
        return Err(format!("{} is not a library.", root.display()));
    }
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

// --- The library menu, from the app's top bar -------------------------------

/// The open library and the others the menu offers.
#[tauri::command]
fn library_menu(app: AppHandle) -> serde_json::Value {
    let current = app
        .state::<DataRoot>()
        .0
        .lock()
        .expect("data root lock")
        .clone();
    let home = home_dir(&app).unwrap_or_default();
    let others: Vec<serde_json::Value> =
        launcher::libraries(&launcher::recents_file(&home), &libraries_dir(&app))
            .into_iter()
            .filter(|l| Some(&l.path) != current.as_ref())
            .map(|l| {
                serde_json::json!({
                    "name": launcher::display_name(&l.path),
                    "path": l.path.to_string_lossy(),
                    "found": l.found,
                })
            })
            .collect();
    serde_json::json!({
        "current": current.map(|p| p.to_string_lossy().into_owned()),
        "others": others,
    })
}

/// Close this library and open another.
#[tauri::command]
fn library_switch(app: AppHandle, path: String) -> Result<(), String> {
    let root = PathBuf::from(path);
    if !launcher::is_data_root(&root) {
        return Err(format!("{} is no longer a data library.", root.display()));
    }
    leave_library(&app).map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn(boot(app, root, false));
    Ok(())
}

/// Close this library and go back to the libraries screen.
#[tauri::command]
fn libraries_show(app: AppHandle) -> Result<(), String> {
    leave_library(&app).map_err(|e| e.to_string())
}

/// Close the open library: the main window goes back to the libraries
/// screen, the library's other windows close, and its server stops.
fn leave_library(app: &AppHandle) -> tauri::Result<()> {
    show_launcher(app)?;
    for (label, window) in app.webview_windows() {
        if label != MAIN_WINDOW {
            let _ = window.destroy();
        }
    }
    let child = app
        .state::<HttpChild>()
        .0
        .lock()
        .expect("http child lock")
        .take();
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    *app.state::<DataRoot>().0.lock().expect("data root lock") = None;
    Ok(())
}

// --- Browse a raw store (src/raw_store.rs) ---------------------------------

/// Returns what the store was opened in, for the page to say.
#[tauri::command]
fn open_raw_store(app: AppHandle, path: String) -> Result<String, String> {
    let root = app
        .state::<DataRoot>()
        .0
        .lock()
        .expect("data root lock")
        .clone()
        .ok_or("No data library is open.")?;
    let store = raw_store::check_store(&root, Path::new(&path))?;
    match raw_store::choose(default_handler(&store)) {
        raw_store::Launch::DbBrowser { app: db_browser } => {
            spawn_open(&raw_store::db_browser_args(&db_browser, &store))?;
            Ok("DB Browser for SQLite".into())
        }
        raw_store::Launch::Shell => {
            let doltlite = resolve_bundled(&app, "datalib-doltlite", "DATALIB_DOLTLITE_BIN")
                .ok_or("The doltlite shell is not bundled with this app.")?;
            let script = write_shell_script(&raw_store::shell_script(&doltlite, &store))?;
            spawn_open(&[script.into()])?;
            Ok("a doltlite shell".into())
        }
    }
}

#[cfg(target_os = "macos")]
fn default_handler(file: &Path) -> Option<raw_store::Handler> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(file.to_str()?));
    let app = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&url)?;
    Some(raw_store::Handler {
        app: PathBuf::from(app.path()?.to_string()),
        bundle_id: NSBundle::bundleWithURL(&app)
            .and_then(|b| b.bundleIdentifier())
            .map(|id| id.to_string()),
    })
}

#[cfg(not(target_os = "macos"))]
fn default_handler(_file: &Path) -> Option<raw_store::Handler> {
    None
}

/// A fresh name per click: Terminal may not have read the last script
/// yet, and each deletes itself once it runs.
fn write_shell_script(body: &str) -> Result<PathBuf, String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "datalib-browse-{}-{}.command",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
fn spawn_open(args: &[std::ffi::OsString]) -> Result<(), String> {
    let out = Command::new("/usr/bin/open")
        .args(args)
        .output()
        .map_err(|e| format!("open: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "open failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(not(target_os = "macos"))]
fn spawn_open(_args: &[std::ffi::OsString]) -> Result<(), String> {
    Err("Opening a raw store is only built for macOS.".into())
}

fn home_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().home_dir().ok()
}

fn libraries_dir(app: &AppHandle) -> PathBuf {
    let home = home_dir(app).expect("the platform names a home directory");
    launcher::libraries_dir(&home)
}

fn main() {
    #[cfg(target_os = "macos")]
    inherit_shell_path();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Backs two things: the grid's "Reveal in Finder" action, and
        // handing an off-origin link to the OS browser (the `↗`
        // outlink and any link inside a rendered document). The webview
        // is granted those two commands and no others — notably not
        // `open_path`, which launches a local file's default
        // application. See `capabilities/reveal-local-files.json` and
        // `capabilities/open-external-urls.json`, which also explain
        // why both need a `remote` block at all (this app loads its UI
        // from localhost as an external URL, and Tauri withholds IPC
        // from remote origins by default).
        // Without `open_js_links_on_click(false)` the plugin injects a
        // click handler that sends every `target="_blank"` link to the OS
        // browser, same-origin included, so `on_new_window` never runs and
        // the card's ↗ lands in a browser with no session cookie.
        // Off-origin links are `ui/src/externalLinks.ts`'s job.
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        // The main window reopens at the size it was closed at, kept in
        // `.window-state.json` in the app's config directory. Card
        // windows are numbered per run, so a saved size would never
        // match one again.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(StateFlags::SIZE | StateFlags::MAXIMIZED)
                .with_filter(|label| label == MAIN_WINDOW)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            version,
            launcher_state,
            launcher_resolve,
            launcher_create,
            launcher_choose_folder,
            launcher_open,
            launcher_pick,
            launcher_forget,
            launcher_open_folder,
            launcher_move_from_documents,
            library_menu,
            library_switch,
            libraries_show,
            open_raw_store
        ])
        .manage(HttpChild(Mutex::new(None)))
        .manage(DataRoot(Mutex::new(None)))
        .manage(AppOrigin(Mutex::new(None)))
        .manage(PageZoom(Mutex::new(zoom::ACTUAL)))
        .on_menu_event(|app, event| match event.id().as_ref() {
            ZOOM_IN => change_zoom(app, zoom::zoom_in),
            ZOOM_OUT => change_zoom(app, zoom::zoom_out),
            ZOOM_ACTUAL => change_zoom(app, |_| zoom::ACTUAL),
            _ => {}
        })
        .setup(|app| {
            let handle = app.handle().clone();
            if let Some(file) = zoom_file(&handle) {
                *handle.state::<PageZoom>().0.lock().unwrap() = zoom::load(&file);
            }
            handle.set_menu(app_menu(&handle)?)?;
            // A data root supplied non-interactively (positional arg or
            // `$DATALIB_DATA_ROOT`) skips the launcher and boots
            // straight into it — mirrors `datalib_http_bin <root>`
            // and makes the app scriptable/testable. Otherwise the
            // libraries screen asks which library to open.
            match explicit_data_root() {
                Some(root) => {
                    tauri::async_runtime::spawn(boot(handle, root, false));
                }
                None => show_launcher(&handle)?,
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building datalib tauri app");

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
/// directories where node and npx live. The backend normally runs
/// latchkey/qmd via the bundled runtime under `Resources/runtime/`
/// (see `datalib_core::node_runtime`) and doesn't need host node —
/// but its `npx` fallback (unstaged version pins, dev-ish setups) still
/// does. Capture the user's login-shell PATH so that fallback keeps
/// working (the spawned `datalib-http` child inherits it), the same
/// trick as the `fix-path-env` crate, without the extra dependency.
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
/// else `$DATALIB_DATA_ROOT`. A leading `~` is expanded against
/// `$HOME` (same convention as `dev.sh`), since `open --env` and shell
/// exports don't do tilde expansion. Returns `None` when neither is set,
/// leaving the interactive picker as the default.
fn explicit_data_root() -> Option<PathBuf> {
    let raw = std::env::args()
        .nth(1)
        .filter(|a| !a.is_empty())
        .or_else(|| std::env::var("DATALIB_DATA_ROOT").ok())
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

/// Show the libraries screen in the main window, opening the window if
/// there is none yet.
fn show_launcher(app: &AppHandle) -> tauri::Result<()> {
    *app.state::<AppOrigin>().0.lock().expect("app origin lock") = None;
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        return window.navigate(launcher_page_url());
    }
    main_window(app, WebviewUrl::App("index.html".into())).build()?;
    Ok(())
}

/// Where the shell serves its bundled page, as a URL to navigate to:
/// a custom scheme on macOS and Linux, a `http://tauri.localhost` host
/// on Windows.
fn launcher_page_url() -> Url {
    let base = if cfg!(windows) {
        "http://tauri.localhost/"
    } else {
        "tauri://localhost/"
    };
    format!("{base}index.html")
        .parse()
        .expect("the bundled page's URL parses")
}

/// The app's one window: the libraries screen, or the open library's
/// page. Its label is what `capabilities/` grant commands to — a window
/// missing from every capability gets no IPC, and the page's first
/// `invoke` fails with nothing on screen to say why.
const MAIN_WINDOW: &str = "main";

/// Wide enough that the toolbar keeps its search box at its min-width
/// and still shows part of the library name, beside the window buttons,
/// back/forward and the sync pill, at comfortable density
/// (`.datalib-toolbar-search` in `datalib/ui/src/App.vue`).
const MIN_WINDOW_WIDTH: f64 = 720.0;
const MIN_WINDOW_HEIGHT: f64 = 480.0;

fn main_window(app: &AppHandle, url: WebviewUrl) -> WebviewWindowBuilder<'_, Wry, AppHandle> {
    app_window(
        WebviewWindowBuilder::new(app, MAIN_WINDOW, url)
            .title("Data Liberation")
            .inner_size(1280.0, 800.0)
            .min_inner_size(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT),
        app,
    )
}

/// Locate a bundled binary. The dev override `$<env>` wins (point it at
/// a fresh Bazel build without rebundling); otherwise the copy bundled
/// under `Contents/Resources/binaries/` (see `tauri.conf.json`
/// `bundle.resources`), which `resource_dir()` resolves regardless of
/// where the bundle lives. The backend finds its own siblings there
/// (`binaries::resolve_binary_dir`), so only what the shell itself
/// runs is looked up here.
fn resolve_bundled(app: &AppHandle, name: &str, env: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!("${env}={} is not a file", p.display());
    }
    let p = app.path().resource_dir().ok()?.join("binaries").join(name);
    p.is_file().then_some(p)
}

/// Start `root`'s server and show it in the main window. `init` writes
/// the starter config first, for a library just created.
async fn boot(app: AppHandle, root: PathBuf, init: bool) {
    remember(&app, &root);
    let url = match tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || start_backend(&app, root, init)
    })
    .await
    {
        Ok(Ok(url)) => url,
        Ok(Err(e)) => return boot_failed(&app, format!("could not start the backend: {e:#}")),
        Err(e) => return boot_failed(&app, format!("backend startup task panicked: {e}")),
    };
    let Ok(url) = url.parse::<Url>() else {
        return boot_failed(&app, format!("backend produced an unusable URL: {url}"));
    };
    *app.state::<AppOrigin>().0.lock().expect("app origin lock") =
        Some(url.origin().ascii_serialization());
    let shown = match app.get_webview_window(MAIN_WINDOW) {
        Some(window) => window.navigate(url),
        None => main_window(&app, WebviewUrl::External(url))
            .build()
            .map(|_| ()),
    };
    if let Err(e) = shown {
        boot_failed(&app, format!("could not open the library: {e}"));
    }
}

/// A boot that did not show its library. The main window is still on
/// the libraries screen, which reloads to take its buttons back.
fn boot_failed(app: &AppHandle, msg: String) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return fatal(app, msg);
    };
    eprintln!("{msg}");
    let _ = window.eval("location.reload()");
    app.dialog()
        .message(msg)
        .title("Datalib could not open that data library")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// Add `root` to the launcher's recent list. Best-effort: a home
/// directory we cannot resolve or write to costs the user a
/// convenience, never a launch.
fn remember(app: &AppHandle, root: &Path) {
    let Some(home) = home_dir(app) else { return };
    if let Err(e) = launcher::record_recent(&launcher::recents_file(&home), root) {
        eprintln!("could not record the recent data root: {e}");
    }
}

/// Windows the app opened on itself, numbered so their labels never
/// collide. `card-*` is what the capability files grant, so a window
/// opened this way can reveal files and pick paths like the main one.
static OPENED_WINDOWS: AtomicUsize = AtomicUsize::new(0);

/// On macOS the page draws its own toolbar where the title bar was:
/// the window's content runs under the bar, the title text is hidden,
/// and the three window buttons sit centred in the toolbar's 40px row.
/// The y is set by eye: the buttons' middle lands about 1pt above it.
/// The UI leaves them room and marks the toolbar as a drag region
/// (`App.vue`, capabilities/window-drag.json).
fn under_title_bar<'a>(
    builder: WebviewWindowBuilder<'a, Wry, AppHandle>,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 21.0));
    builder
}

/// The two rules every window of the app follows.
///
/// **A window shows the app, never someone else's website.** Rendered
/// documents carry links we did not author — the `↗` outlink, and every
/// `<a>` that came out of the source content (a "Sent via Superhuman"
/// footer, a newsletter's tracking link). Following one in place would
/// replace the whole UI with a marketing page and leave no chrome to
/// come back from, so an off-origin navigation goes to the OS browser.
///
/// **A `target="_blank"` link opens a window.** A webview has no tab
/// strip, so without this the card chrome's "open this card alone" ↗
/// and the grid's double-click were dead in the app. Same origin gets a
/// second window of the app, under the same rules; anything else goes
/// to the OS browser, as above.
///
/// The app's origin is read at each navigation, not fixed when the
/// window opens: the main window moves between the libraries screen and
/// each library's server, and each server has a port of its own.
fn app_window<'a>(
    builder: WebviewWindowBuilder<'a, Wry, AppHandle>,
    app: &AppHandle,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    let nav_app = app.clone();
    let new_app = app.clone();
    let zoom_app = app.clone();
    under_title_bar(builder)
        // Tauri's file-drop handler claims every drag over the webview,
        // so WebKit never fires a page's own dragover or drop: the grids'
        // drag-a-column-to-group bar (SortableJS) did nothing. Nothing
        // here listens for Tauri's drop events. Without it, a file
        // dropped from Finder is WebKit's to open: see `on_navigation`.
        .disable_drag_drop_handler()
        // A new page starts at the webview's default zoom; put it back at
        // the View menu's.
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.set_zoom(page_zoom(&zoom_app));
            }
        })
        .on_navigation(move |next| {
            // A file dropped on the window: WebKit would show it in place
            // of the app, with no way back.
            if next.scheme() == "file" {
                return false;
            }
            if !leaves_the_app(next, &nav_app) {
                return true;
            }
            open_externally(&nav_app, next);
            false
        })
        .on_new_window(move |url, _features: NewWindowFeatures| {
            if leaves_the_app(&url, &new_app) {
                open_externally(&new_app, &url);
            } else {
                open_card_window(&new_app, url);
            }
            NewWindowResponse::Deny
        })
}

/// A second window of the app at `url`, built afresh rather than handed
/// back to WebKit as the answer to `window.open`. That answer must use
/// the opener's configuration, scripts included, and Tauri's script
/// naming the window (`__TAURI_INTERNALS__.metadata`) cannot be
/// redefined, so the opener's runs first and wins: the new window
/// believed it was `main`, and dragging its toolbar moved the main
/// window. Both windows use the default website data store, so they
/// still share the session cookie. Built on a task, not in the
/// callback: Tauri warns that building a window from a synchronous
/// handler deadlocks on Windows.
fn open_card_window(app: &AppHandle, url: Url) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let label = format!("card-{}", OPENED_WINDOWS.fetch_add(1, Ordering::Relaxed));
        let built = app_window(
            WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url.clone()))
                .title("Data Liberation")
                .inner_size(1100.0, 760.0)
                .min_inner_size(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT),
            &app,
        )
        .build();
        if let Err(e) = built {
            eprintln!("could not open a window for {url}: {e}");
        }
    });
}

fn open_externally(app: &AppHandle, url: &Url) {
    if let Err(e) = app.opener().open_url(url.as_str(), None::<&str>) {
        eprintln!("could not open {url} externally: {e}");
    }
}

/// The libraries screen's own page counts as the app: on Windows it is
/// served from `http://tauri.localhost`.
fn leaves_the_app(next: &Url, app: &AppHandle) -> bool {
    let origin = next.origin().ascii_serialization();
    let app_origin = app
        .state::<AppOrigin>()
        .0
        .lock()
        .expect("app origin lock")
        .clone();
    let launcher_origin = launcher_page_url().origin().ascii_serialization();
    match next.scheme() {
        "http" | "https" => Some(&origin) != app_origin.as_ref() && origin != launcher_origin,
        "mailto" | "tel" => true,
        _ => false,
    }
}

/// Spawn the bundled `datalib-http` against `root` on an ephemeral
/// localhost port and wait (≤15s) for it to announce its URL via
/// `--url-file`. The child's output goes to a log file in the temp dir
/// so startup failures can quote it in the error dialog (a
/// Finder-launched app has no terminal). Blocking: run on a worker
/// thread, not the event loop.
fn start_backend(app: &AppHandle, root: PathBuf, init: bool) -> anyhow::Result<String> {
    let http_bin = resolve_bundled(app, "datalib-http", "DATALIB_HTTP_BIN").ok_or_else(|| {
        anyhow::anyhow!(
            "datalib-http binary not found (no bundled copy and \
             $DATALIB_HTTP_BIN not set)"
        )
    })?;

    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let url_file = tmp.join(format!("datalib-http-{pid}.url"));
    let log_file = tmp.join(format!("datalib-http-{pid}.log"));
    // Remove a stale url-file from a recycled PID so we can't read a
    // dead server's address.
    let _ = std::fs::remove_file(&url_file);

    let log = std::fs::File::create(&log_file)
        .map_err(|e| anyhow::anyhow!("create backend log {}: {e}", log_file.display()))?;
    // The backend announces its launch URL — which carries the API
    // token — on stderr, and that lands in this file, in a temp dir
    // that is shared between users on Linux. Owner-only. (The url-file
    // itself is tightened by the backend for the same reason.)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&log_file, std::fs::Permissions::from_mode(0o600));
    }
    let log_err = log
        .try_clone()
        .map_err(|e| anyhow::anyhow!("clone backend log handle: {e}"))?;

    let mut child = Command::new(&http_bin)
        .arg(&root)
        .arg("--no-open")
        .arg("--url-file")
        .arg(&url_file)
        .args(init.then_some("--init"))
        .env("DATALIB_BIND", "127.0.0.1:0")
        // The backend exits when this pipe hits EOF, which the kernel
        // arranges however the shell goes — the `kill` at exit is for
        // the ways it can still run code, this is for the ones it
        // can't. `child` keeps the write end; never `take()` it.
        .env("DATALIB_PARENT_PIPE", "0")
        .stdin(Stdio::piped())
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
                "datalib-http exited during startup ({status}):\n{}",
                log_tail(&log_file)
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "datalib-http did not announce its URL within 15s \
                 (log: {}):\n{}",
                log_file.display(),
                log_tail(&log_file)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = std::fs::remove_file(&url_file);

    *app.state::<HttpChild>().0.lock().expect("http child lock") = Some(child);
    *app.state::<DataRoot>().0.lock().expect("data root lock") = Some(root);
    Ok(url)
}

fn log_tail(path: &std::path::Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::from("(no backend log captured)");
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(20);
    lines[start..].join("\n")
}

fn fatal(app: &AppHandle, msg: String) {
    eprintln!("{msg}");
    let handle = app.clone();
    app.dialog()
        .message(msg)
        .title("Datalib failed to start")
        .kind(MessageDialogKind::Error)
        .show(move |_| handle.exit(1));
}
