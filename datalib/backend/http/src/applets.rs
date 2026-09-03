//! Applets: config-declared servers that contribute endpoints, and
//! that write their frontend components into the store.
//!
//! An applet is a sibling of a step in `config.toml` (see
//! [`datalib_dag::config::AppletEntry`]). Where a step runs to
//! completion during a sync and writes artifacts, an applet is a
//! long-lived HTTP server this gateway starts. It is run once:
//!
//! ```text
//! <command> -p 0 --frontend-dir <root>/system/frontend/<id> [--params <json>]
//! ```
//!
//! and owes three things, in order: write its components into that
//! directory, bind a port, then print `DATALIB_APPLET_PORT=<port>` to
//! stdout. The gateway waits for that line and then scans the store,
//! so **the line is the signal that the write finished** — an applet
//! that announced first would race the scan and intermittently come up
//! with no components.
//!
//! The port travels in that direction — child to gateway, not gateway
//! to child — because it is the only spelling that ties readiness to
//! *this* child. Picking a port here means binding one, dropping it,
//! and racing the child for it; the wait that followed could then only
//! ask "is anything accepting on that port?", which another process
//! that won the race answers just as well. That is not hypothetical:
//! under a loaded `bazelisk test //...` the gateway adopted a
//! stranger's listener, scanned the store before its own applet had
//! written a byte, and reported no error while the real child exited
//! with `EADDRINUSE`.
//!
//! Beyond that one line there is no protocol version, no handshake,
//! and no registration call.
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
//! That is why the components come from a directory rather than an
//! endpoint: reading them must not require asking an applet anything,
//! or the gallery could not list a component until something already
//! knew to open it.
//!
//! ## Why the applet is told its own directory
//!
//! Two instances of one command differ only in their config. Passing
//! the destination is what lets each write its own namespace — and
//! what lets each bake its own id into the `component_args` of the
//! gallery entry it registers, so the two appear as separate rows over
//! one shared component.
//!
//! ## Applets are started eagerly and kept running
//!
//! Every configured applet is started at boot and stays up. The
//! alternative — starting one on its first request — cannot work now
//! that the write and the serve are one invocation: components would
//! only exist once something had already opened a card that used them,
//! which is the thing the gallery needs them for.
//!
//! So a data root with twelve applets runs twelve processes. Idle
//! shutdown would trade some of that back and is not built; if it
//! arrives, a restarted applet simply rewrites the same files, since
//! the write is idempotent.
//!
//! ## A config reload restarts only what changed
//!
//! The registry remembers the applet list it last started. When
//! `config.toml` moves, the new list is compared against that record
//! entry by entry. An entry spelled exactly the same way, whose
//! process is still alive, keeps running untouched. Everything else is
//! stopped and started again: an entry whose config changed, one that
//! is new, and one whose process has died since it was started.
//!
//! Restarting an applet the edit had nothing to do with is not free —
//! it throws away whatever the process holds in memory, and the thing
//! that notices the config moved is a UI poll of `/api/frontend`, so
//! an unrelated edit would interrupt every applet at once.
//!
//! ## Starting an applet is destructive to its namespace
//!
//! An applet about to start gets a clean namespace: its directory is
//! deleted first and it rewrites it, so a component it no longer emits
//! actually disappears. Every directory belonging to no configured
//! applet is deleted too, which is what takes the components of a
//! removed applet with it. `user` is never touched, which is the whole
//! reason that id is reserved
//! ([`datalib_dag::config::RESERVED_APPLET_ID`]).
//!
//! A kept applet's directory is left exactly as it is. It would be
//! rewritten byte-for-byte anyway — the write is idempotent for
//! unchanged config — so deleting it would only open a window where
//! the gallery could scan a namespace that is missing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_dag::config::AppletEntry;
use serde::Serialize;

/// The applet's own id, as the gateway knows it. The reference applet
/// uses it to label its data; anything building an absolute URL should
/// prefer [`ENV_APPLET_BASE`].
pub const ENV_APPLET_ID: &str = "DATALIB_APPLET_ID";

/// The prefix the gateway proxies to this applet (`/applet/<id>/`). An
/// applet that emits absolute URLs must build them from this rather
/// than assuming the mount layout.
pub const ENV_APPLET_BASE: &str = "DATALIB_APPLET_BASE";

/// The prefix of the one line an applet prints to **stdout** once it
/// has written its components and bound its port: the readiness
/// signal, carrying the port the gateway proxies to.
///
/// The applet side spells this literally (`datalib-applet`'s
/// `announce_port`), the same way it spells [`ENV_APPLET_ID`] — the
/// two cannot drift silently, since every applet round-trip test
/// starts a real child and fails outright if they disagree.
pub const APPLET_PORT_LINE: &str = "DATALIB_APPLET_PORT=";

/// How long an applet gets to write its components, bind its port, and
/// report it.
///
/// A bound is required because this runs during boot, after the
/// listener is already accepting: without one, a single applet that
/// hangs would leave a browser tab whose requests queue forever with
/// nothing logged.
const START_TIMEOUT: Duration = Duration::from_secs(20);

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
    cmd.env(ENV_APPLET_BASE, format!("/applet/{}/", entry.id));
    for (k, v) in &entry.env {
        cmd.env(k, v);
    }
    Ok(cmd)
}

/// The frontend store root, created and marked as derived.
fn frontend_root(data_root: &Path) -> anyhow::Result<PathBuf> {
    let root = crate::frontend::frontend_dir(data_root);
    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", root.display()))?;
    // Everything under here is reproducible by restarting the applets,
    // so cache-aware backups may skip it — except `user`, which is not.
    // Marking the parent is close enough: the tag is advisory.
    datalib_core::layout::mark_derived_cache(&root);
    Ok(root)
}

/// Delete every applet-owned namespace directory except the ones in
/// `keep`.
///
/// Deleting is what makes the store track the config: an applet
/// removed from `config.toml` is in neither `keep` nor the list about
/// to be started, so it leaves no orphaned components behind, and a
/// component dropped from a restarting applet's output actually
/// disappears. `user` is never touched, which is the whole reason that
/// id is reserved.
fn prune_namespaces(root: &Path, keep: &BTreeSet<String>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|s| s.to_str()) {
            Some(crate::frontend::USER_NAMESPACE) | None => continue,
            Some(name) if keep.contains(name) => continue,
            Some(_) => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
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

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The applets from `config.toml`, the frontend store they write into,
/// and the child processes behind `/applet/`.
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
    /// The applet list as of the last start — the record a config
    /// reload diffs against to decide what has to restart.
    entries: Vec<AppletEntry>,
    /// The directory the last start resolved commands against. Kept
    /// alongside `entries` because it is half of what an entry means:
    /// the same `command = "datalib-applet slack"` is a different
    /// program under a different `binary_dir`, so a change here
    /// invalidates every entry at once.
    binary_dir: Option<PathBuf>,
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
    /// `binary_dir` is the directory applet commands resolve against,
    /// already resolved — [`Self::from_data_root`] is what turns a CLI
    /// override plus the config's own `binary_dir` into one.
    ///
    /// A failing applet does not fail the boot: its error is recorded
    /// and everything else still loads. `user` is scanned either way,
    /// since nothing regenerates it.
    pub fn build(
        entries: Vec<AppletEntry>,
        data_root: PathBuf,
        binary_dir: Option<PathBuf>,
    ) -> Self {
        Self::new(entries, data_root, binary_dir.clone(), binary_dir)
    }

    /// `binary_dir_override` is the CLI's, kept for later reloads;
    /// `binary_dir` is what this start actually resolves against. They
    /// differ whenever the config supplies its own.
    fn new(
        entries: Vec<AppletEntry>,
        data_root: PathBuf,
        binary_dir_override: Option<PathBuf>,
        binary_dir: Option<PathBuf>,
    ) -> Self {
        let supervisor = Supervisor::default();
        // Nothing is running yet, so every entry starts and every
        // applet namespace is rebuilt.
        let errors = reconcile(
            &supervisor,
            &[],
            &entries,
            &data_root,
            binary_dir.as_deref(),
        );
        let store = crate::frontend::FrontendStore::scan(&data_root);
        let config_stamp = config_stamp_of(&data_root);
        let store_stamp = crate::frontend::StoreStamp::of(&data_root);
        Self {
            data_root,
            binary_dir_override,
            state: std::sync::RwLock::new(RegistryState {
                store_stamp,
                entries,
                binary_dir,
                store,
                errors,
                config_stamp,
            }),
            supervisor,
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
        // The *resolved* dir is what the start uses, the same one a
        // later reload will resolve. Handing `build` the bare override
        // instead would make boot resolve commands one way and the
        // first config edit resolve them another.
        let (entries, resolved) = load_entries(data_root, binary_dir.clone());
        Self::new(entries, data_root.to_path_buf(), binary_dir, resolved)
    }

    /// Reconcile the running applets with `config.toml` if it has
    /// changed since the last pass.
    ///
    /// Blocking: it execs one child per applet that has to start.
    /// Callers on the async side run it inside `spawn_blocking`. Cheap
    /// when nothing moved — one `stat` and a read lock.
    pub fn refresh_if_config_changed(&self) {
        let current = config_stamp_of(&self.data_root);
        let (prev_entries, prev_binary_dir) = {
            let Ok(state) = self.state.read() else { return };
            if state.config_stamp == current {
                // The config is unchanged, but the store may not be —
                // a `PUT /api/lib`, or a file dropped in by hand.
                drop(state);
                self.rescan_if_store_changed();
                return;
            }
            (state.entries.clone(), state.binary_dir.clone())
        };
        let (entries, binary_dir) = load_entries(&self.data_root, self.binary_dir_override.clone());

        // A different `binary_dir` can resolve every command to a
        // different program, so no entry survives that change however
        // unchanged its own text is. Passing an empty `prev` is how
        // that is said: nothing matches, everything restarts.
        let prev: &[AppletEntry] = if binary_dir == prev_binary_dir {
            &prev_entries
        } else {
            &[]
        };
        let errors = reconcile(
            &self.supervisor,
            prev,
            &entries,
            &self.data_root,
            binary_dir.as_deref(),
        );
        let store = crate::frontend::FrontendStore::scan(&self.data_root);

        let store_stamp = crate::frontend::StoreStamp::of(&self.data_root);
        if let Ok(mut state) = self.state.write() {
            state.entries = entries;
            state.binary_dir = binary_dir;
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
        // Configured but not running is a different failure from not
        // configured at all, and the message says which.
        let port = match self.supervisor.port(id) {
            Some(p) => p,
            None if self.entry(id).is_some() => {
                let why = self
                    .state
                    .read()
                    .ok()
                    .and_then(|s| s.errors.get(id).cloned())
                    .unwrap_or_else(|| "it is not running".to_string());
                return Err(format!("applet {id:?}: {why}"));
            }
            // Not in the list. That has two very different causes, and
            // they used to produce the same message: the applet really
            // isn't configured, or the config could not be read at all
            // — which yields an empty list and so reports every applet
            // as missing. Saying "no applet \"unified_index\"" when the
            // truth is "your config.toml has a syntax error" sends
            // people looking in exactly the wrong place.
            None => {
                return Err(match config_load_error(&self.data_root, id) {
                    Some(why) => format!(
                        "applet {id:?} is unavailable because {} could not be loaded: {why}",
                        datalib_dag::config::root_config_path(&self.data_root).display()
                    ),
                    None => format!("no applet {id:?}"),
                })
            }
        };
        forward(port, method, path_and_query, content_type, body)
    }
}

/// The whole frontend, as one document.
#[derive(Debug, Default, Serialize)]
pub struct FrontendView {
    /// namespace → its components and any files it could not use.
    pub namespaces: BTreeMap<String, crate::frontend::NamespaceView>,
    /// Applets that failed to start. Their namespace is absent, since
    /// an applet writes it as it comes up; saying which one broke beats
    /// an empty gallery.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub applet_errors: BTreeMap<String, String>,
}

/// Bring the running applets in line with `next`, given `prev` — the
/// list that is currently running.
///
/// An entry in both lists, spelled identically, whose process is still
/// alive, is left alone: not stopped, not started, its namespace
/// directory not touched. Everything else in `next` is started, and
/// everything running that `next` does not keep is stopped. Pass an
/// empty `prev` to restart the lot.
///
/// Starts run on threads so a reload is bounded by the slowest applet
/// rather than their sum: a broken one costs the readiness timeout
/// once, not once per applet ahead of it in the list.
///
/// Returns one message per applet that failed to start, so a broken
/// applet is visible instead of just absent. A kept applet contributes
/// no entry — it is running, which is the only thing an error here
/// means.
fn reconcile(
    supervisor: &Supervisor,
    prev: &[AppletEntry],
    next: &[AppletEntry],
    data_root: &Path,
    binary_dir: Option<&Path>,
) -> BTreeMap<String, String> {
    // `port` is also the liveness check, and it reaps: an applet that
    // died since it was started reports gone here and gets started
    // again, rather than being kept with a port nothing is listening
    // on. That also means a config edit retries an applet that failed
    // to start last time.
    let keep: BTreeSet<String> = next
        .iter()
        .filter(|e| prev.contains(e) && supervisor.port(&e.id).is_some())
        .map(|e| e.id.clone())
        .collect();

    supervisor.stop_except(&keep);

    let to_start: Vec<&AppletEntry> = next.iter().filter(|e| !keep.contains(&e.id)).collect();

    let root = match frontend_root(data_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("applets: {e:#}");
            return to_start
                .iter()
                .map(|e2| (e2.id.clone(), format!("{e:#}")))
                .collect();
        }
    };
    prune_namespaces(&root, &keep);

    let mut errors = BTreeMap::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = to_start
            .iter()
            .map(|entry| {
                let dir = root.join(&entry.id);
                scope.spawn(move || {
                    (
                        entry.id.clone(),
                        supervisor.start(entry, data_root, binary_dir, &dir),
                    )
                })
            })
            .collect();
        for h in handles {
            if let Ok((id, Err(e))) = h.join() {
                eprintln!("applet {id}: {e}");
                errors.insert(id, e);
            }
        }
    });
    errors
}

/// Why the applet a request asked for is not in the list, when the
/// config is the reason — `None` when the config is fine, or simply
/// isn't there yet, which is the normal state of a fresh root rather
/// than an error.
///
/// Two shapes qualify, and they are different sentences. The file may
/// not be a config at all, in which case no applet exists. Or the file
/// loaded and *this applet's entry* was dropped, which is the case the
/// graded loader introduced: everything else works, and the one thing
/// the caller wanted does not.
///
/// Only called on a failure path, so re-reading the file here costs
/// nothing worth caching and keeps the reason next to the request that
/// needs it.
fn config_load_error(data_root: &Path, id: &str) -> Option<String> {
    let path = datalib_dag::config::root_config_path(data_root);
    if !path.exists() {
        return None;
    }
    let (checked, _) = datalib_dag::config::load_graded(&path).ok()?;
    if let Some(d) = checked
        .diagnostics
        .iter()
        .find(|d| d.id() == Some(id) && d.severity.drops_the_entry())
    {
        return Some(d.describe());
    }
    checked.is_fatal().then(|| {
        checked
            .diagnostics
            .first()
            .map_or_else(String::new, |d| d.describe())
    })
}

/// Read and validate the applet list out of a data root's config.
fn load_entries(
    data_root: &Path,
    binary_dir: Option<PathBuf>,
) -> (Vec<AppletEntry>, Option<PathBuf>) {
    let cfg_path = datalib_dag::config::root_config_path(data_root);
    match datalib_dag::config::load_graded(&cfg_path) {
        Ok((checked, _)) => {
            // Resolve `binary_dir` exactly the way the DAG runner does,
            // so `command = "datalib-applet slack"` finds the same binary
            // whether a step or an applet names it.
            let dir = datalib_dag::config::resolve_binary_dir(&checked.cfg, binary_dir.as_deref());
            // `checked.cfg.applets` is already only the entries that
            // loaded. This used to be all-or-nothing — one bad applet
            // entry logged "config rejected, none will load" and the
            // whole app went dark, which is 00633dd5 and the reason
            // #209 exists. A dropped entry now costs its own applet.
            //
            // Still said out loud: an applet that is silently absent is
            // how the original mystery started, and `config_load_error`
            // says it again on the request that trips over it.
            // Only the applet ones: a broken *step* is the runner's to
            // report, and echoing it here would put it in the log twice
            // under a heading that has nothing to do with it.
            for d in checked.diagnostics.iter().filter(|d| {
                d.entry.as_ref().map(|e| e.kind) == Some(datalib_dag::EntryKind::Applet)
                    || d.severity == datalib_dag::Severity::Fatal
            }) {
                eprintln!("applets: {}", d.describe());
            }
            (checked.cfg.applets, dir)
        }
        // No config yet is the normal state of a fresh data root; it is
        // not an error and must not stop the server. A file we cannot
        // even read is a different thing entirely.
        Err(e) => {
            if cfg_path.exists() {
                eprintln!("applets: config could not be read, none will start: {e:#}");
            }
            (Vec::new(), binary_dir)
        }
    }
}

// ---------------------------------------------------------------------------
// Supervision
// ---------------------------------------------------------------------------

struct Running {
    port: u16,
    child: Child,
}

/// The applet servers, all of them, started at boot and kept running.
///
/// There is no lazy start: an applet writes its components as it comes
/// up, so deferring the start until something requested the applet
/// would mean its components did not exist until something already knew
/// to ask for them.
#[derive(Default)]
pub struct Supervisor {
    running: Mutex<BTreeMap<String, Running>>,
}

impl Supervisor {
    /// Start one applet and wait for it to report the port it bound.
    ///
    /// Returns that port. The wait is what makes the caller's
    /// subsequent store scan safe: the applet writes its directory,
    /// binds, and only then prints the line, so having read the line
    /// means the files are there.
    ///
    /// The port comes back *from the child* rather than being picked
    /// here, and that is the whole point. Choosing one in this process
    /// means binding it, releasing it, and hoping the child wins the
    /// race for it — and the readiness check that followed ("something
    /// accepts on that port") could not tell this applet apart from
    /// whoever else had grabbed it. Under load that actually happened:
    /// the gateway adopted a stranger's listener, scanned the store
    /// before the applet had written a byte, and served an empty
    /// gallery while the real child died of `EADDRINUSE` unreported.
    fn start(
        &self,
        entry: &AppletEntry,
        data_root: &Path,
        binary_dir: Option<&Path>,
        frontend_dir: &Path,
    ) -> Result<u16, String> {
        let mut cmd = base_command(entry, data_root, binary_dir)
            .map_err(|e| format!("applet {:?}: {e:#}", entry.id))?;
        // `0` means "any port": the child asks the OS for one and
        // reports what it got.
        cmd.arg("-p")
            .arg("0")
            .arg("--frontend-dir")
            .arg(frontend_dir);
        if let Some(params) = entry
            .params_json()
            .map_err(|e| format!("applet {:?}: params: {e:#}", entry.id))?
        {
            cmd.arg("--params").arg(
                serde_json::to_string(&params)
                    .map_err(|e| format!("applet {:?}: params → JSON: {e}", entry.id))?,
            );
        }
        // stdin is not an input channel — nothing is ever written to
        // it. It is a liveness pipe, and the applet's only way to find
        // out that this gateway is gone.
        //
        // The write end lives in the `Child` we hold, so it stays open
        // exactly as long as this process does. Whatever ends us —
        // an orderly exit, a SIGTERM, a SIGKILL that runs no code at
        // all — the kernel closes it, and the applet's read end goes
        // to EOF. That is the one signal that survives SIGKILL, which
        // is why the handler in `main` is not enough by itself.
        //
        // `DATALIB_APPLET_PARENT_PIPE` is how the applet knows this
        // stdin means that. Set here rather than assumed there because
        // an applet is an ordinary program someone may run by hand:
        // reading stdin unbidden would swallow a terminal's input, and
        // treating an immediate EOF from `< /dev/null` as "my parent
        // died" would make it exit at once. See docs/dev/applets.md.
        cmd.stdin(Stdio::piped());
        cmd.env("DATALIB_APPLET_PARENT_PIPE", "1");
        // stdout is the readiness channel; stderr is the log, captured
        // so a server that dies on startup can say why. Without the
        // latter the only symptom is a readiness failure, which names
        // the applet and nothing about the cause.
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("applet {:?}: spawn {:?}: {e}", entry.id, entry.command))?;

        // Drain stderr on a detached thread that both forwards each
        // line and keeps the tail in a shared buffer.
        //
        // Detached, and read without joining, on purpose: the pipe is
        // held by the child *and every process it spawned*, so killing
        // a failed applet does not necessarily close it. A `sh` wrapper
        // whose own child is still alive would otherwise block the
        // reader — and with it this whole function — until that
        // grandchild exited. `stderr_eof` is how the failure path waits
        // for the tail to be complete without giving up that property.
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let (stderr_eof_tx, stderr_eof) = std::sync::mpsc::channel::<()>();
        if let Some(stderr) = child.stderr.take() {
            let tail = tail.clone();
            let id = entry.id.clone();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("applet {id}: {line}");
                    if let Ok(mut t) = tail.lock() {
                        t.push(line);
                        // Bounded: this lives as long as the applet.
                        if t.len() > 40 {
                            t.drain(..20);
                        }
                    }
                }
                let _ = stderr_eof_tx.send(());
            });
        }

        // Read stdout on its own detached thread, for the same reason,
        // and forward the first `DATALIB_APPLET_PORT=` line here. The
        // thread keeps draining afterwards: a chatty applet that filled
        // the pipe would otherwise block on its own logging forever.
        let (ready_tx, ready) = std::sync::mpsc::channel::<Option<u16>>();
        if let Some(stdout) = child.stdout.take() {
            let id = entry.id.clone();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                let mut announced = false;
                for line in reader.lines().map_while(Result::ok) {
                    let port = (!announced)
                        .then(|| line.trim().strip_prefix(APPLET_PORT_LINE))
                        .flatten()
                        .and_then(|p| p.trim().parse::<u16>().ok())
                        .filter(|p| *p != 0);
                    match port {
                        Some(port) => {
                            announced = true;
                            let _ = ready_tx.send(Some(port));
                        }
                        // Anything else on stdout is just output. An
                        // applet is not required to keep it clean.
                        None => eprintln!("applet {id}: {line}"),
                    }
                }
                if !announced {
                    let _ = ready_tx.send(None);
                }
            });
        }

        let port = match ready.recv_timeout(START_TIMEOUT) {
            Ok(Some(port)) => port,
            // `None` is EOF with nothing announced — the applet
            // exited, or closed stdout, before it was ready. Either
            // way it is not something this gateway can proxy to.
            // `Err` is the timeout, or the reader thread going away.
            outcome => {
                // Kill *and reap*: `std::process::Child` has no reaping
                // Drop, so a bare `kill` leaves a zombie for the
                // gateway's lifetime.
                let _ = child.kill();
                let _ = child.wait();
                // An applet that closed stdout has usually just died,
                // and its last words — the reason — may still be in the
                // stderr pipe. Wait for that reader to finish so the
                // message is complete rather than whatever had arrived
                // by now. Bounded, because a surviving grandchild can
                // hold the pipe open; skipped entirely on the timeout
                // path, where the child is hung rather than dying and
                // there is nothing more to come.
                if matches!(
                    outcome,
                    Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                ) {
                    let _ = stderr_eof.recv_timeout(Duration::from_secs(2));
                }
                let collected = tail.lock().map(|t| t.join("\n")).unwrap_or_default();
                let detail = if collected.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", tail_lines(&collected, 20))
                };
                let why = match outcome {
                    Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        "exited without reporting a listening port".to_string()
                    }
                    _ => format!(
                        "did not report a listening port within {}s",
                        START_TIMEOUT.as_secs()
                    ),
                };
                return Err(format!("applet {:?}: {why}{detail}", entry.id));
            }
        };

        if let Ok(mut map) = self.running.lock() {
            map.insert(entry.id.clone(), Running { port, child });
        }
        Ok(port)
    }

    /// The port an applet is listening on, if it is running.
    fn port(&self, id: &str) -> Option<u16> {
        let mut map = self.running.lock().ok()?;
        let r = map.get_mut(id)?;
        // A child that exited leaves a stale port that would
        // connection-refuse on every request; reap it and report gone.
        match r.child.try_wait() {
            Ok(None) => Some(r.port),
            _ => {
                if let Some(mut dead) = map.remove(id) {
                    let _ = dead.child.wait();
                }
                None
            }
        }
    }

    /// Stop every applet whose id is not in `keep`, reaping as it
    /// goes.
    fn stop_except(&self, keep: &BTreeSet<String>) {
        let Ok(mut map) = self.running.lock() else {
            return;
        };
        let doomed: Vec<String> = map
            .keys()
            .filter(|id| !keep.contains(id.as_str()))
            .cloned()
            .collect();
        for id in doomed {
            if let Some(mut r) = map.remove(&id) {
                let _ = r.child.kill();
                // Reap: `kill` only signals, and an unwaited child stays
                // a zombie until its parent exits.
                let _ = r.child.wait();
            }
        }
    }

    /// Stop every applet.
    fn stop_all(&self) {
        self.stop_except(&BTreeSet::new());
    }
}

impl AppletRegistry {
    /// Stop every applet this gateway started.
    ///
    /// The same thing `Supervisor`'s `Drop` does, reachable by name —
    /// because `Drop` only runs when the process ends of its own
    /// accord, and the usual way a gateway ends is a signal. See the
    /// shutdown handler in `datalib-http`'s `main`.
    pub fn shutdown(&self) {
        self.supervisor.stop_all();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

// `Drop` alone was never enough, and the two gaps needed different
// answers (#238):
//
//   * It does not run on a signal. The gateway had no handler, so a
//     SIGTERM stopped it mid-instruction and nothing here was reached.
//     `datalib-http`'s `main` now serves with a graceful shutdown and
//     calls `AppletRegistry::shutdown` on the way out.
//   * It cannot run on SIGKILL, ever. Nothing in this process does. So
//     the applet is given a pipe on stdin instead — see the spawn
//     above — and exits when it reads EOF, which the kernel delivers
//     however this process happens to die.
//
// Process groups would still be worth having, so that killing an
// applet also takes anything the applet itself spawned. They do not
// replace either of the above: signalling a group needs somebody alive
// to send the signal, and after a SIGKILL there is nobody.

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
