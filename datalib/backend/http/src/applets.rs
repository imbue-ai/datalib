//! Applets: config-declared servers that contribute endpoints, and
//! that write their frontend components into the store.
//!
//! An applet is a sibling of a step in `config.toml` (see
//! [`datalib_dag::config::AppletEntry`]). Where a step runs to
//! completion during a sync and writes artifacts, an applet is a
//! long-lived HTTP server this gateway spawns on demand. It owes the
//! gateway two things:
//!
//! ```text
//! <command> --write-frontend-dir <root>/system/frontend/<id>   # then exit
//! <command> -p <port>                                          # then serve
//! ```
//!
//! There is no protocol version, no handshake, and no registration
//! call, so a shell script is a viable applet.
//!
//! ## Applets are not a component mechanism
//!
//! Everything about *components* lives in [`crate::frontend`], which
//! knows nothing about applets. An applet's only privilege is being
//! **called** to write a directory; the files it leaves behind are
//! scanned, hash-validated and served exactly like ones a user dropped
//! in by hand. One mechanism, one code path, nothing for the two to
//! disagree about.
//!
//! That is why the write is a flag and not an endpoint: components have
//! to be readable before any applet is worth running, so opening the
//! app costs zero applet processes. A server starts only when a card
//! actually asks one for data.
//!
//! ## Why the applet is told its own directory
//!
//! Two instances of one command differ only in their config. Passing
//! the destination is what lets each write its own namespace — and
//! what lets each bake its own id into the `component_args` of the
//! gallery entry it registers, so the two appear as separate rows over
//! one shared component.
//!
//! ## Refresh is destructive
//!
//! A refresh deletes every namespace directory except `user` and asks
//! the applets to rewrite theirs. That is what keeps the store honest
//! when an applet is removed from the config — its components go with
//! it — and it is why `user` is refused as an applet id
//! ([`datalib_dag::config::RESERVED_APPLET_ID`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use datalib_dag::config::AppletEntry;
use serde::Serialize;

/// The applet's own id, as the gateway knows it. The reference applet
/// uses it to label its data; anything building an absolute URL should
/// prefer [`ENV_APPLET_BASE`].
pub const ENV_APPLET_ID: &str = "DATALIB_APPLET_ID";

/// The prefix the gateway proxies to this applet (`/v/<id>/`). An
/// applet that emits absolute URLs must build them from this rather
/// than assuming the mount layout.
pub const ENV_APPLET_BASE: &str = "DATALIB_APPLET_BASE";

/// How long an applet gets to write its namespace and exit.
///
/// Without a bound, a command that starts *serving* when asked to
/// *write* — an easy mistake for a binary with both modes — would hang
/// the refresh, and a refresh runs during boot after the listener is
/// already bound. The symptom would be a browser tab whose requests
/// queue forever with nothing logged.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Split a `command` string into an argv the way the DAG runner does —
/// literally the same `shlex` call, so the two config entries cannot
/// drift apart on quoting.
fn split_command(id: &str, command: &str) -> anyhow::Result<Vec<String>> {
    let argv = shlex::split(command).ok_or_else(|| {
        anyhow::anyhow!("applet {id:?}: command {command:?} has unbalanced quoting")
    })?;
    if argv.is_empty() {
        anyhow::bail!("applet {id:?}: empty command");
    }
    Ok(argv)
}

/// Build the child command shared by the manifest dump and the server:
/// argv from config, cwd at the data root, `binary_dir` prepended to
/// PATH, and the entry's `env` merged last so it wins.
fn base_command(
    entry: &AppletEntry,
    data_root: &Path,
    binary_dir: Option<&Path>,
) -> anyhow::Result<Command> {
    let argv = split_command(&entry.id, &entry.command)?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(data_root);
    if let Some(path) = child_path(binary_dir, crate::user_bin_dir())
        .map_err(|e| anyhow::anyhow!("applet {:?}: build child PATH: {e}", entry.id))?
    {
        cmd.env("PATH", path);
    }
    // `DATALIB_DAG_DATA_ROOT` is the step protocol's established
    // spelling for this value, and reusing it keeps one name for one
    // thing. The unprefixed `DATALIB_DATA_ROOT` is already taken: the
    // Tauri shell reads it as the user's chosen root, so an applet
    // under a Tauri-hosted gateway would see it set by two unrelated
    // mechanisms.
    cmd.env(datalib_dag::subprocess::ENV_DATA_ROOT, data_root);
    cmd.env(ENV_APPLET_ID, &entry.id);
    cmd.env(ENV_APPLET_BASE, format!("/v/{}/", entry.id));
    for (k, v) in &entry.env {
        cmd.env(k, v);
    }
    Ok(cmd)
}

/// Ask one applet to write its frontend namespace.
///
/// The applet writes files; it prints nothing this function reads.
/// Anything it does say goes to stderr and becomes the error message,
/// the same convention a failed step follows.
pub fn write_frontend_dir(
    entry: &AppletEntry,
    data_root: &Path,
    binary_dir: Option<&Path>,
    dir: &Path,
) -> anyhow::Result<()> {
    write_frontend_dir_with_timeout(entry, data_root, binary_dir, dir, WRITE_TIMEOUT)
}

/// [`write_frontend_dir`] with an explicit bound, so a test can prove
/// the timeout exists without waiting out the production one.
pub fn write_frontend_dir_with_timeout(
    entry: &AppletEntry,
    data_root: &Path,
    binary_dir: Option<&Path>,
    dir: &Path,
    timeout: Duration,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;

    let mut cmd = base_command(entry, data_root, binary_dir)?;
    cmd.arg("--write-frontend-dir").arg(dir);
    if let Some(params) = entry.params_json()? {
        cmd.arg("--params").arg(serde_json::to_string(&params)?);
    }
    cmd.stdin(Stdio::null());

    let out = run_with_timeout(cmd, timeout)
        .map_err(|e| anyhow::anyhow!("applet {:?}: {:?}: {e}", entry.id, entry.command))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "applet {:?}: --write-frontend-dir exited {}: {}",
            entry.id,
            out.status,
            tail_lines(&stderr, 20)
        );
    }
    Ok(())
}

/// Rebuild every applet-owned namespace.
///
/// Deletes each namespace directory that is not `user`, then asks each
/// configured applet to write its own. Deleting first is what makes the
/// store track the config: an applet removed from `config.toml` leaves
/// no orphaned components behind, and a component removed from an
/// applet's output actually disappears. `user` is never touched, which
/// is the whole reason that id is reserved.
///
/// Returns one message per applet that failed, so a broken applet is
/// visible instead of just absent.
pub fn refresh_frontend(
    entries: &[AppletEntry],
    data_root: &Path,
    binary_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let root = crate::frontend::frontend_dir(data_root);
    if let Err(e) = std::fs::create_dir_all(&root) {
        return vec![(String::new(), format!("create {}: {e}", root.display()))];
    }
    // Everything under here is reproducible by re-running the applets,
    // so cache-aware backups may skip it — except `user`, which is not.
    // Marking the parent is close enough: the tag is advisory.
    datalib_core::layout::mark_derived_cache(&root);

    if let Ok(rd) = std::fs::read_dir(&root) {
        for ent in rd.flatten() {
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            match path.file_name().and_then(|s| s.to_str()) {
                Some(crate::frontend::USER_NAMESPACE) | None => continue,
                Some(_) => {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }

    let mut errors = Vec::new();
    for e in entries {
        let dir = root.join(&e.id);
        if let Err(err) = write_frontend_dir(e, data_root, binary_dir, &dir) {
            eprintln!("applet {}: {err:#}", e.id);
            errors.push((e.id.clone(), format!("{err:#}")));
        }
    }
    errors
}

/// The PATH an applet child sees: `binary_dir`, then `~/.datalib/bin`,
/// then whatever this process inherited.
///
/// That order matches what a *step* gets, which is the point. A step's
/// child sees `binary_dir` first (the DAG runner prepends it) over a
/// PATH the sync worker has already prefixed with `~/.datalib/bin`, so
/// an applet resolving its command differently from a step would make
/// `/agent/config.md`'s "install it in ~/.datalib/bin" advice true for
/// one kind of config entry and false for the other.
///
/// `join_paths`, not a hardcoded separator, so this is correct on
/// Windows and preserves non-UTF-8 components. Returns `None` only when
/// there is nothing to prepend and no PATH to inherit.
fn child_path(
    binary_dir: Option<&Path>,
    user_bin: Option<PathBuf>,
) -> Result<Option<std::ffi::OsString>, std::env::JoinPathsError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(dir) = binary_dir {
        paths.push(dir.to_path_buf());
    }
    if let Some(dir) = user_bin {
        // Prepended even when the dir does not exist yet: an agent may
        // create it between runs, and a missing PATH entry is harmless.
        // Same reasoning as the worker's.
        if Some(dir.as_path()) != binary_dir {
            paths.push(dir);
        }
    }
    let inherited = std::env::var_os("PATH");
    if paths.is_empty() && inherited.is_none() {
        return Ok(None);
    }
    if let Some(p) = &inherited {
        paths.extend(std::env::split_paths(p));
    }
    std::env::join_paths(paths).map(Some)
}

/// Run a command to completion, killing and reaping it if it outlives
/// `timeout`.
///
/// `Command::output` waits forever, which is the wrong shape for a
/// contract whose whole point is "print and exit": a command that
/// starts serving instead — an easy mistake, since the same binary has
/// a serving mode — would otherwise hang the refresh, and a refresh runs
/// during boot after the listener is already bound. The user would get
/// a tab whose requests queue in the backlog with nothing logged.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> anyhow::Result<std::process::Output> {
    // Pipe both streams: the manifest arrives on stdout and the
    // failure explanation on stderr.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

    // Drain the pipes on threads. Reading inline would deadlock the
    // moment a chatty command fills a pipe buffer while we wait.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reap, so the corpse does not linger as a zombie
                    // for the gateway's lifetime.
                    let _ = child.wait();
                    anyhow::bail!(
                        "--write-frontend-dir did not exit within {:?} (it must write its files \
                         and exit; is this the serving mode?)",
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => anyhow::bail!("wait: {e}"),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out_h.join().unwrap_or_default(),
        stderr: err_h.join().unwrap_or_default(),
    })
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The applets from `config.toml`, the frontend store they write into,
/// and the child processes behind `/v/`.
///
/// Rebuilt lazily when `config.toml` changes: every read path calls
/// [`Self::refresh_if_config_changed`], which costs one `stat` and does
/// nothing unless the file moved. Watching the mtime rather than
/// hooking `PUT /api/config` means a config edited by hand — or by an
/// agent writing the file directly — is picked up too, and the UI's
/// poll of `/api/frontend` turns that into a live update.
///
/// Note what this type does *not* do: it holds no component data of its
/// own. Components come from [`crate::frontend::FrontendStore`], which
/// reads the filesystem and cannot tell who wrote it.
pub struct AppletRegistry {
    pub data_root: PathBuf,
    /// The CLI's `--binary-dir`, kept so a rebuild can re-resolve
    /// against a config whose own `binary_dir` may have changed.
    binary_dir_override: Option<PathBuf>,
    state: std::sync::RwLock<RegistryState>,
    supervisor: Supervisor,
}

struct RegistryState {
    entries: Vec<AppletEntry>,
    store: crate::frontend::FrontendStore,
    /// applet id → why its write failed, for the ones that did.
    errors: BTreeMap<String, String>,
    /// Size and mtime of `config.toml` as of the last rebuild. `None`
    /// when the file was absent, which is itself worth remembering:
    /// creating it has to count as a change.
    config_stamp: Option<(u64, std::time::SystemTime)>,
    /// Shape of the frontend tree as of the last scan. Separate from
    /// the config stamp because the two trigger different work: a
    /// config change re-runs every applet, while a file appearing in
    /// the store only needs a rescan. Conflating them would make a
    /// `PUT /api/lib` wipe and rewrite every applet namespace.
    store_stamp: crate::frontend::StoreStamp,
}

/// What the config file looks like right now, for change detection.
/// Size *and* mtime, because a same-size same-second rewrite is
/// plausible for a small hand-edited file.
fn config_stamp_of(data_root: &Path) -> Option<(u64, std::time::SystemTime)> {
    let path = datalib_dag::config::root_config_path(data_root);
    let md = std::fs::metadata(path).ok()?;
    Some((md.len(), md.modified().ok()?))
}

impl AppletRegistry {
    /// Run every applet's write, then scan the store.
    ///
    /// A failing applet does not fail the boot: its error is recorded
    /// and everything else still loads. `user` is scanned either way,
    /// since nothing regenerates it.
    pub fn build(
        entries: Vec<AppletEntry>,
        data_root: PathBuf,
        binary_dir: Option<PathBuf>,
    ) -> Self {
        let errors = refresh_frontend(&entries, &data_root, binary_dir.as_deref())
            .into_iter()
            .collect();
        let store = crate::frontend::FrontendStore::scan(&data_root);
        let config_stamp = config_stamp_of(&data_root);
        let store_stamp = crate::frontend::StoreStamp::of(&data_root);
        Self {
            data_root,
            binary_dir_override: binary_dir,
            state: std::sync::RwLock::new(RegistryState {
                store_stamp,
                entries,
                store,
                errors,
                config_stamp,
            }),
            supervisor: Supervisor::default(),
        }
    }

    /// Read `config.toml` and build from it.
    ///
    /// The policy lives here rather than in `boot`: a missing config is
    /// the normal state of a fresh data root and yields no applets, and
    /// a config the validator rejects also yields none — a server that
    /// refused to start over a bad applet id would take search and
    /// setup down with it, leaving no way to fix the file.
    pub fn from_data_root(data_root: &Path, binary_dir: Option<PathBuf>) -> Self {
        let (entries, _) = load_entries(data_root, binary_dir.clone());
        Self::build(entries, data_root.to_path_buf(), binary_dir)
    }

    /// Rebuild if `config.toml` has changed since the last pass.
    ///
    /// Blocking: it execs one child per applet. Callers on the async
    /// side run it inside `spawn_blocking`. Cheap when nothing moved —
    /// one `stat` and a read lock.
    pub fn refresh_if_config_changed(&self) {
        let current = config_stamp_of(&self.data_root);
        {
            let Ok(state) = self.state.read() else { return };
            if state.config_stamp == current {
                // The config is unchanged, but the store may not be —
                // a `PUT /api/lib`, or a file dropped in by hand.
                drop(state);
                self.rescan_if_store_changed();
                return;
            }
        }
        let (entries, bin_dir) = load_entries(&self.data_root, self.binary_dir_override.clone());

        // Stop the servers whose config changed (or vanished) so the
        // next request respawns them with the new params. Leaving them
        // running would silently serve the old `params` — a config edit
        // that appears to take effect in the gallery but not in the
        // data is worse than a restart.
        let changed: Vec<String> = {
            let Ok(state) = self.state.read() else { return };
            entries
                .iter()
                .filter(|e| state.entries.iter().find(|p| p.id == e.id) != Some(e))
                .map(|e| e.id.clone())
                .chain(
                    state
                        .entries
                        .iter()
                        .filter(|p| !entries.iter().any(|e| e.id == p.id))
                        .map(|p| p.id.clone()),
                )
                .collect()
        };
        for id in &changed {
            self.supervisor.stop(id);
        }

        let errors = refresh_frontend(&entries, &self.data_root, bin_dir.as_deref())
            .into_iter()
            .collect();
        let store = crate::frontend::FrontendStore::scan(&self.data_root);

        let store_stamp = crate::frontend::StoreStamp::of(&self.data_root);
        if let Ok(mut state) = self.state.write() {
            state.entries = entries;
            state.store = store;
            state.errors = errors;
            state.config_stamp = current;
            state.store_stamp = store_stamp;
        }
    }

    /// Rescan the frontend tree if anything in it moved.
    ///
    /// The store is the source of truth, so a component written by
    /// `PUT /api/lib` — or a directory a person dropped in by hand —
    /// has to show up without a config edit or a restart. This costs a
    /// handful of `stat`s when nothing changed, and re-reads the tree
    /// when it did; it never runs an applet.
    pub fn rescan_if_store_changed(&self) {
        let current = crate::frontend::StoreStamp::of(&self.data_root);
        {
            let Ok(state) = self.state.read() else { return };
            if state.store_stamp == current {
                return;
            }
        }
        let store = crate::frontend::FrontendStore::scan(&self.data_root);
        if let Ok(mut state) = self.state.write() {
            state.store = store;
            state.store_stamp = current;
        }
    }

    /// The frontend as `GET /api/frontend` reports it: every namespace
    /// the store found, plus the applets that failed to write theirs.
    pub fn frontend_view(&self) -> FrontendView {
        let Ok(state) = self.state.read() else {
            return FrontendView::default();
        };
        FrontendView {
            namespaces: state.store.view().clone(),
            applet_errors: state.errors.clone(),
        }
    }

    /// Read a component's bytes by content hash.
    pub fn read_component(&self, hash: &str) -> Option<Vec<u8>> {
        self.state.read().ok()?.store.read_component(hash)
    }

    fn entry(&self, id: &str) -> Option<AppletEntry> {
        self.state
            .read()
            .ok()?
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }

    /// Proxy one request to an applet, starting it if it is not already
    /// running.
    pub fn proxy(
        &self,
        id: &str,
        method: &str,
        path_and_query: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<ProxyResponse, String> {
        let entry = self.entry(id).ok_or_else(|| format!("no applet {id:?}"))?;
        let binary_dir = self.binary_dir_for_spawn();
        let port = self
            .supervisor
            .ensure(&entry, &self.data_root, binary_dir.as_deref())?;
        forward(port, method, path_and_query, content_type, body)
    }

    /// `binary_dir` as the current config resolves it.
    fn binary_dir_for_spawn(&self) -> Option<PathBuf> {
        let cfg_path = datalib_dag::config::root_config_path(&self.data_root);
        match datalib_dag::config::load(&cfg_path) {
            Ok((cfg, _)) => {
                datalib_dag::config::resolve_binary_dir(&cfg, self.binary_dir_override.as_deref())
            }
            Err(_) => self.binary_dir_override.clone(),
        }
    }
}

/// The whole frontend, as one document.
#[derive(Debug, Default, Serialize)]
pub struct FrontendView {
    /// namespace → its components and any files it could not use.
    pub namespaces: BTreeMap<String, crate::frontend::NamespaceView>,
    /// Applets whose `--write-frontend-dir` failed. Their namespace is
    /// absent or stale; saying which one broke beats an empty gallery.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub applet_errors: BTreeMap<String, String>,
}

/// Read and validate the applet list out of a data root's config.
fn load_entries(
    data_root: &Path,
    binary_dir: Option<PathBuf>,
) -> (Vec<AppletEntry>, Option<PathBuf>) {
    let cfg_path = datalib_dag::config::root_config_path(data_root);
    match datalib_dag::config::load(&cfg_path) {
        Ok((cfg, _)) => {
            // Resolve `binary_dir` exactly the way the DAG runner does,
            // so `command = "datalib-view-slack"` finds the same binary
            // whether a step or an applet names it.
            let dir = datalib_dag::config::resolve_binary_dir(&cfg, binary_dir.as_deref());
            match datalib_dag::config::validate_applets(&cfg) {
                Ok(()) => (cfg.applets, dir),
                Err(e) => {
                    eprintln!("applets: config rejected, none will load: {e:#}");
                    (Vec::new(), dir)
                }
            }
        }
        // No config yet is the normal state of a fresh data root; it is
        // not an error and must not stop the server.
        Err(_) => (Vec::new(), binary_dir),
    }
}

// ---------------------------------------------------------------------------
// Supervision
// ---------------------------------------------------------------------------

struct Running {
    port: u16,
    child: Child,
}

/// Lazily-spawned applet servers, one per id.
///
/// Deliberately simple: spawn on first use, keep alive, and let the
/// process die with the server. Idle shutdown and restart-with-backoff
/// are the obvious next increments; neither changes the interface.
#[derive(Default)]
pub struct Supervisor {
    running: Mutex<BTreeMap<String, Running>>,
    /// Applets whose last start attempt failed, with the reason.
    ///
    /// Without this, `ensure` retries on every single request: each
    /// one spawns a child, waits out the readiness timeout, and kills
    /// it. A card that polls would turn a broken applet into a stream
    /// of ten-second requests and one corpse apiece. Remembering the
    /// failure makes the second request fail immediately with the
    /// first one's reason.
    failed: Mutex<BTreeMap<String, String>>,
}

impl Supervisor {
    /// Return the port this applet is listening on, spawning it first
    /// if needed.
    fn ensure(
        &self,
        entry: &AppletEntry,
        data_root: &Path,
        binary_dir: Option<&Path>,
    ) -> Result<u16, String> {
        let mut map = self
            .running
            .lock()
            .map_err(|_| "supervisor poisoned".to_string())?;
        if let Some(r) = map.get_mut(&entry.id) {
            // A child that exited leaves a stale port that would
            // connection-refuse on every request; reap it and respawn.
            match r.child.try_wait() {
                Ok(None) => return Ok(r.port),
                _ => {
                    if let Some(mut dead) = map.remove(&entry.id) {
                        let _ = dead.child.wait();
                    }
                }
            }
        }
        // A previous start failed. Report that rather than paying the
        // readiness timeout again on every request.
        if let Ok(failed) = self.failed.lock() {
            if let Some(why) = failed.get(&entry.id) {
                return Err(format!("{why} (cached; restart datalib-http to retry)"));
            }
        }
        let port = free_port().map_err(|e| format!("applet {:?}: no free port: {e}", entry.id))?;
        let mut cmd = base_command(entry, data_root, binary_dir)
            .map_err(|e| format!("applet {:?}: {e:#}", entry.id))?;
        cmd.arg("-p").arg(port.to_string());
        if let Some(params) = entry
            .params_json()
            .map_err(|e| format!("applet {:?}: params: {e:#}", entry.id))?
        {
            cmd.arg("--params").arg(
                serde_json::to_string(&params)
                    .map_err(|e| format!("applet {:?}: params → JSON: {e}", entry.id))?,
            );
        }
        cmd.stdin(Stdio::null());
        // Capture stderr so a server that dies on startup can say why.
        // Without this the only symptom is a readiness timeout, which
        // names the port and nothing about the cause — the same
        // dead-end the step protocol avoids by making a failed step's
        // last stderr lines its error message.
        cmd.stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("applet {:?}: spawn {:?}: {e}", entry.id, entry.command))?;
        let stderr = child.stderr.take();
        let err_h = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut s) = stderr {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        });
        let mut running = Running { port, child };
        if let Err(e) = wait_ready(port, Duration::from_secs(10)) {
            // Kill *and reap*: `std::process::Child` has no reaping
            // Drop, so a bare `kill` leaves a zombie for the gateway's
            // lifetime.
            let _ = running.child.kill();
            let _ = running.child.wait();
            // The pipe closes when the child dies, so the reader
            // thread finishes and the tail is available now.
            let stderr = err_h.join().unwrap_or_default();
            let detail = if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("; stderr: {}", tail_lines(&stderr, 20))
            };
            let why = format!(
                "applet {:?}: did not start listening on {port}: {e}{detail}",
                entry.id
            );
            if let Ok(mut failed) = self.failed.lock() {
                failed.insert(entry.id.clone(), why.clone());
            }
            return Err(why);
        }
        // The applet came up. Its stderr keeps draining on that
        // thread; forward it so an applet's logs are not swallowed for
        // the life of the process.
        let id_for_log = entry.id.clone();
        std::thread::spawn(move || {
            let text = err_h.join().unwrap_or_default();
            for line in text.lines() {
                eprintln!("applet {id_for_log}: {line}");
            }
        });
        map.insert(entry.id.clone(), running);
        Ok(port)
    }
}

impl Supervisor {
    /// Stop one applet if it is running, so the next request respawns
    /// it. Used when its config entry changed under us.
    fn stop(&self, id: &str) {
        if let Ok(mut map) = self.running.lock() {
            if let Some(mut r) = map.remove(id) {
                let _ = r.child.kill();
                // Reap: `kill` only signals, and an unwaited child stays
                // a zombie until its parent exits.
                let _ = r.child.wait();
            }
        }
        // A previously-failed start is no longer authoritative either.
        if let Ok(mut failed) = self.failed.lock() {
            failed.remove(id);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Ok(mut map) = self.running.lock() {
            for (_, mut r) in std::mem::take(&mut *map) {
                let _ = r.child.kill();
                // Reap: kill only signals, and an unwaited child stays
                // a zombie until its parent exits.
                let _ = r.child.wait();
            }
        }
    }
}

// NOTE: `Drop` runs on an orderly shutdown, not when the gateway is
// SIGKILLed — in that case the applet children are re-parented to init
// and keep running until their idle logic (which does not exist yet)
// would stop them. Putting each child in its own process group and
// signalling the group is the fix; it is not done here because idle
// shutdown will need the same plumbing.

/// Ask the OS for a free port by binding one and letting it go. There
/// is a race between release and the applet's bind, which is why the
/// alternative (pass `-p 0` and have the applet report back) is the
/// better long-term shape — it needs a readiness channel this protocol
/// deliberately does not have yet.
fn free_port() -> std::io::Result<u16> {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

fn wait_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    Err(last)
}

// ---------------------------------------------------------------------------
// The proxy
// ---------------------------------------------------------------------------

pub struct ProxyResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Forward one request over a fresh HTTP/1.1 connection and read the
/// whole response.
///
/// Public so a test can drive it against a listener it controls and
/// assert the exact bytes on the wire — this is a hand-written client,
/// so its framing is worth pinning rather than inferring from a
/// round trip.
///
/// Hand-rolled rather than pulling in an HTTP client: the workspace
/// has no client crate in its Bazel dep set, and adding one means
/// repinning crate_universe. The cost is real and worth naming — this
/// buffers the entire response and speaks no chunked *request* bodies,
/// keep-alive, or upgrades. It is enough for a JSON API and not enough
/// for streaming, which is the first thing to revisit when an applet
/// wants server-sent events.
pub fn forward(
    port: u16,
    method: &str,
    path_and_query: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<ProxyResponse, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("connect 127.0.0.1:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;

    let mut req = format!(
        "{method} {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        // Carry the caller's own content type. Hardcoding JSON here
        // would mislabel every form, text, or binary POST — and the
        // route accepts any method, so those are in scope.
        req.push_str(&format!(
            "Content-Type: {}\r\n",
            content_type.unwrap_or("application/octet-stream")
        ));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .map_err(|e| format!("write body: {e}"))?;
    }
    stream.flush().map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read response: {e}"))?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<ProxyResponse, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "applet response had no header terminator".to_string())?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("applet response has no status: {status_line:?}"))?;
    let mut content_type = "application/octet-stream".to_string();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-type") {
                content_type = v.trim().to_string();
            }
        }
    }
    Ok(ProxyResponse {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `split_command` wraps the same `shlex::split` the DAG runner
    /// uses for a step, so the two entry kinds cannot disagree about
    /// quoting. What is worth pinning here is the error framing: a
    /// broken `command` must name the applet, since that is the only
    /// way the user knows which config entry to fix.
    #[test]
    fn split_command_accepts_shell_quoting() {
        assert_eq!(
            split_command("a", r#"prog "two words" 'and more'"#).unwrap(),
            vec!["prog", "two words", "and more"]
        );
    }

    #[test]
    fn split_command_names_the_applet_when_it_fails() {
        let err = split_command("slack_work", r#"prog "unbalanced"#).unwrap_err();
        assert!(err.to_string().contains("slack_work"), "{err}");
        let err = split_command("slack_work", "   ").unwrap_err();
        assert!(err.to_string().contains("empty command"), "{err}");
    }

    /// The order is the contract: an applet must resolve a bare command
    /// the same way a step does, or the documented install location
    /// only works for half the config file.
    #[test]
    fn child_path_prepends_binary_dir_then_the_user_bin_dir() {
        let bin = PathBuf::from("/opt/datalib/bin");
        let user = PathBuf::from("/home/u/.datalib/bin");
        let joined = child_path(Some(&bin), Some(user.clone()))
            .unwrap()
            .expect("some path");
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(parts[0], bin);
        assert_eq!(parts[1], user);
        // …and the inherited PATH still follows, so packaged binaries
        // stay reachable.
        assert!(parts.len() > 2, "inherited PATH was dropped: {parts:?}");
    }

    #[test]
    fn child_path_does_not_repeat_a_dir() {
        let dir = PathBuf::from("/home/u/.datalib/bin");
        let joined = child_path(Some(&dir), Some(dir.clone()))
            .unwrap()
            .expect("some path");
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(parts.iter().filter(|p| **p == dir).count(), 1, "{parts:?}");
    }

    /// With no config `binary_dir`, `~/.datalib/bin` still comes first —
    /// this is the case the user's own config hits.
    #[test]
    fn child_path_works_without_a_binary_dir() {
        let user = PathBuf::from("/home/u/.datalib/bin");
        let joined = child_path(None, Some(user.clone()))
            .unwrap()
            .expect("some path");
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(parts[0], user);
    }

    #[test]
    fn parses_a_minimal_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        assert_eq!(r.body, b"{\"ok\":true}");
    }

    #[test]
    fn reports_a_missing_header_terminator() {
        assert!(parse_response(b"HTTP/1.1 200 OK").is_err());
    }
}
