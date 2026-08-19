//! Applets: config-declared servers that contribute the app's frontend
//! components and the endpoints behind them.
//!
//! An applet is a sibling of a step in `config.toml` (see
//! [`datalib_dag::config::AppletEntry`]). Where a step runs to
//! completion during a sync and writes artifacts, an applet is a
//! long-lived HTTP server this gateway spawns on demand. It owes the
//! gateway three things:
//!
//! 1. `--frontend-manifest --applet-id <id> --module-dir <dir>` —
//!    write its component modules into `<dir>` (each file named after
//!    the sha256 of its own bytes) and print a manifest on stdout.
//!    Runs to completion; no server involved.
//! 2. `-p <port>` — bind `127.0.0.1:<port>` and serve its API.
//! 3. Nothing else. There is no protocol version, no handshake, and no
//!    registration call.
//!
//! ## Why the manifest is a flag and not an endpoint
//!
//! Three things need the manifest before any applet is worth running:
//! the component gallery, the registry that resolves a name in card
//! source, and the module URLs the browser imports from. Making it a
//! flag means opening the app costs zero applet processes — a server
//! starts only when a card actually asks one for data.
//!
//! ## Why the applet is told its own id
//!
//! Gallery entries are *full card-source snippets*, not names, and a
//! snippet has to address the instance it came from
//! (`slack_work.channels("slack_work")`). Two instances of one command
//! differ only in their config, so the id has to arrive from outside.
//!
//! ## The module store
//!
//! Every applet's modules land in one flat, content-addressed
//! directory served at `/modules/<sha256>`. Two instances of the same
//! command write identical bytes to the same name, so the write is
//! idempotent and needs no arbitration — and, because the browser
//! keeps one module instance per resolved URL, the two instances share
//! one evaluated module without the gateway doing anything. Drifted
//! builds simply produce two hashes and stop sharing, which is correct
//! rather than a special case.

use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::sha256_hex;
use datalib_dag::config::AppletEntry;
use serde::{Deserialize, Serialize};

/// The applet's own id, as the gateway knows it. The reference applet
/// uses it to label its data; anything building an absolute URL should
/// prefer [`ENV_APPLET_BASE`].
pub const ENV_APPLET_ID: &str = "DATALIB_APPLET_ID";

/// The prefix the gateway proxies to this applet (`/v/<id>/`). An
/// applet that emits absolute URLs must build them from this rather
/// than assuming the mount layout.
pub const ENV_APPLET_BASE: &str = "DATALIB_APPLET_BASE";

/// How long an applet gets to print its manifest and exit. Without a
/// bound, a command that starts serving instead of exiting would hang
/// discovery — and with it the whole boot — forever.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where an applet's modules are dropped and served from. A derived
/// cache: everything in it is reproducible by re-running the manifest
/// dump, so backups may skip it and a sweep may empty it.
pub fn module_store_dir(data_root: &Path) -> PathBuf {
    data_root.join("system").join("modules")
}

// ---------------------------------------------------------------------------
// The manifest an applet prints
// ---------------------------------------------------------------------------

/// What `--frontend-manifest` prints on stdout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrontendManifest {
    /// Components this applet contributes, each naming a module in the
    /// store. The name is a member of the applet's namespace, not a
    /// global: `slack_work.channels`. It therefore only has to be
    /// unique within this one manifest, which is why nothing here
    /// arbitrates collisions between applets — they cannot occur.
    #[serde(default)]
    pub components: Vec<ComponentEntry>,
    /// Ready-to-use card sources for the new-card gallery. Opaque to
    /// the gateway and to the UI: whatever string is here is what a
    /// picked entry writes into the card. Keeping it a snippet rather
    /// than a name is what lets one component appear in the gallery
    /// several times with different arguments.
    #[serde(default)]
    pub gallery: Vec<GalleryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentEntry {
    /// Member name inside the applet's namespace.
    pub name: String,
    /// sha256 of the module's bytes; the file's name in the store and
    /// the last path segment of its `/modules/<hash>` URL.
    pub module: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryEntry {
    /// Full card source, e.g. `slack_work.channels("slack_work")`.
    pub source: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

// ---------------------------------------------------------------------------
// What the UI is told
// ---------------------------------------------------------------------------

/// One applet as `GET /api/applets` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct AppletView {
    pub id: String,
    pub title: String,
    /// component name → module hash. The UI turns each into an
    /// `import("/modules/<hash>")` and hangs the result off `id`.
    pub components: BTreeMap<String, String>,
    pub gallery: Vec<GalleryEntry>,
    /// Set when discovery failed. The applet still appears — a
    /// configured-but-broken applet the user can see is better than a
    /// silent omission that looks like a config that never saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

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

/// Run `--frontend-manifest` for one applet and fold its modules into
/// the store.
///
/// The applet writes module files itself (it knows its own bytes); the
/// gateway re-hashes every file the manifest points at before trusting
/// the name. That turns "two applets disagree about a hash" from a
/// race into an impossibility, and catches a build that forgot to
/// re-hash after changing a module — which would otherwise serve stale
/// code forever from an immutable URL.
pub fn discover_one(
    entry: &AppletEntry,
    data_root: &Path,
    binary_dir: Option<&Path>,
    store: &Path,
) -> anyhow::Result<FrontendManifest> {
    discover_one_with_timeout(entry, data_root, binary_dir, store, MANIFEST_TIMEOUT)
}

/// [`discover_one`] with an explicit bound, so a test can prove the
/// timeout exists without waiting out the production one.
pub fn discover_one_with_timeout(
    entry: &AppletEntry,
    data_root: &Path,
    binary_dir: Option<&Path>,
    store: &Path,
    timeout: Duration,
) -> anyhow::Result<FrontendManifest> {
    std::fs::create_dir_all(store)
        .map_err(|e| anyhow::anyhow!("create module store {}: {e}", store.display()))?;
    datalib_core::layout::mark_derived_cache(store);

    let mut cmd = base_command(entry, data_root, binary_dir)?;
    cmd.arg("--frontend-manifest")
        .arg("--applet-id")
        .arg(&entry.id)
        .arg("--module-dir")
        .arg(store);
    if let Some(params) = entry.params_json()? {
        cmd.arg("--params").arg(serde_json::to_string(&params)?);
    }
    cmd.stdin(Stdio::null());

    let out = run_with_timeout(cmd, timeout)
        .map_err(|e| anyhow::anyhow!("applet {:?}: {:?}: {e}", entry.id, entry.command))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "applet {:?}: --frontend-manifest exited {}: {}",
            entry.id,
            out.status,
            tail_lines(&stderr, 20)
        );
    }
    let manifest: FrontendManifest = serde_json::from_slice(&out.stdout).map_err(|e| {
        anyhow::anyhow!(
            "applet {:?}: manifest is not valid JSON: {e}; got {:?}",
            entry.id,
            String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(200)
                .collect::<String>()
        )
    })?;

    for c in &manifest.components {
        verify_module(&entry.id, store, c)?;
    }
    Ok(manifest)
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
/// a serving mode — would otherwise hang discovery, and discovery runs
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
                        "--frontend-manifest did not exit within {:?} (a manifest dump must print \
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

/// A module named by a hash must actually hash to that name.
fn verify_module(id: &str, store: &Path, c: &ComponentEntry) -> anyhow::Result<()> {
    if !is_sha256_hex(&c.module) {
        anyhow::bail!(
            "applet {id:?}: component {:?} has module {:?}, which is not a sha256 hex digest",
            c.name,
            c.module
        );
    }
    let path = store.join(&c.module);
    let bytes = std::fs::read(&path).map_err(|e| {
        anyhow::anyhow!(
            "applet {id:?}: component {:?} names module {} but {} is unreadable: {e}",
            c.name,
            c.module,
            path.display()
        )
    })?;
    let actual = sha256_hex(&bytes);
    if actual != c.module {
        anyhow::bail!(
            "applet {id:?}: component {:?} claims module {} but its bytes hash to {} — \
             the build wrote a file under the wrong name",
            c.name,
            c.module,
            actual
        );
    }
    Ok(())
}

/// Guard before a hash is ever joined onto a directory, so a request
/// path cannot traverse out of the store. Same shape check the card
/// store applies on read.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Everything the gateway knows about the configured applets.
///
/// Built once, at server start. Nothing re-runs discovery when
/// `config.toml` changes, so a new applet needs a restart today; the
/// UI polls `/api/applets` and its change detection is in place for
/// when that is wired to `PUT /api/config`. Holds no child processes
/// itself — those live in [`Supervisor`].
pub struct AppletRegistry {
    pub data_root: PathBuf,
    pub binary_dir: Option<PathBuf>,
    pub store: PathBuf,
    entries: Vec<AppletEntry>,
    views: Vec<AppletView>,
    supervisor: Supervisor,
}

impl AppletRegistry {
    /// Discover every configured applet. A failing applet does not
    /// fail the boot: it lands in the registry carrying its error, so
    /// the UI can say which one is broken and why.
    pub fn discover(
        entries: Vec<AppletEntry>,
        data_root: PathBuf,
        binary_dir: Option<PathBuf>,
    ) -> Self {
        let store = module_store_dir(&data_root);
        let mut views = Vec::with_capacity(entries.len());
        for e in &entries {
            let view = match discover_one(e, &data_root, binary_dir.as_deref(), &store) {
                Ok(m) => AppletView {
                    id: e.id.clone(),
                    title: e.display_title().to_string(),
                    components: m
                        .components
                        .iter()
                        .map(|c| (c.name.clone(), c.module.clone()))
                        .collect(),
                    gallery: m.gallery,
                    error: None,
                },
                Err(err) => {
                    eprintln!("applet {}: discovery failed: {err:#}", e.id);
                    AppletView {
                        id: e.id.clone(),
                        title: e.display_title().to_string(),
                        components: BTreeMap::new(),
                        gallery: Vec::new(),
                        error: Some(format!("{err:#}")),
                    }
                }
            };
            views.push(view);
        }
        Self {
            data_root,
            binary_dir,
            store,
            entries,
            views,
            supervisor: Supervisor::default(),
        }
    }

    /// Build the registry for a data root: read its `config.toml`,
    /// resolve `binary_dir` the way the DAG runner does, validate, and
    /// discover.
    ///
    /// The policy lives here rather than in `boot`: a missing config
    /// is the normal state of a fresh data root and yields no applets,
    /// and a config the validator rejects also yields none — a server
    /// that refuses to start over a bad applet id would take search
    /// and setup down with it, leaving no way to fix the file.
    pub fn from_data_root(data_root: &Path, binary_dir: Option<PathBuf>) -> Self {
        let cfg_path = datalib_dag::config::root_config_path(data_root);
        let (entries, bin_dir) = match datalib_dag::config::load(&cfg_path) {
            Ok((cfg, _)) => {
                let dir = datalib_dag::config::resolve_binary_dir(&cfg, binary_dir.as_deref());
                match datalib_dag::config::validate_applets(&cfg) {
                    Ok(()) => (cfg.applets, dir),
                    Err(e) => {
                        eprintln!("applets: config rejected, none will load: {e:#}");
                        (Vec::new(), dir)
                    }
                }
            }
            Err(_) => (Vec::new(), binary_dir),
        };
        Self::discover(entries, data_root.to_path_buf(), bin_dir)
    }

    pub fn views(&self) -> &[AppletView] {
        &self.views
    }

    pub fn entry(&self, id: &str) -> Option<&AppletEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Read a module out of the store. `hash` is validated before it
    /// touches the filesystem.
    pub fn read_module(&self, hash: &str) -> Option<Vec<u8>> {
        if !is_sha256_hex(hash) {
            return None;
        }
        std::fs::read(self.store.join(hash)).ok()
    }

    /// Proxy one request to an applet, starting it if it is not
    /// already running.
    pub fn proxy(
        &self,
        id: &str,
        method: &str,
        path_and_query: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<ProxyResponse, String> {
        let entry = self.entry(id).ok_or_else(|| format!("no applet {id:?}"))?;
        let port = self
            .supervisor
            .ensure(entry, &self.data_root, self.binary_dir.as_deref())?;
        forward(port, method, path_and_query, content_type, body)
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
    fn hash_shape_is_checked_before_a_path_join() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"A".repeat(64)));
        assert!(!is_sha256_hex("../etc/passwd"));
        assert!(!is_sha256_hex(&"a".repeat(63)));
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
