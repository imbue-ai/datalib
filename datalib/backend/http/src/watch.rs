//! What changed in the data root, pushed instead of polled.
//!
//! Rides one SSE connection, as named `root` frames, so a client has one
//! connection, one reconnect policy, and one heartbeat to judge liveness by. A frame names a *dataset* a reader fetches, not the
//! file that moved: one file can feed several readers, and one file —
//! the run store — is written by two processes for two audiences. The
//! filesystem says a file moved; the store says which of its parts did
//! (`datalib_runs::versions`); this module turns both into the datasets
//! to fetch again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use datalib_runs::StorePart;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// How often to publish a [`RootEvent::Heartbeat`] on an otherwise
/// silent stream.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// The watch's clocks, which a test sets to zero.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long to hold a burst of filesystem events before publishing.
    pub debounce: Duration,
    /// The least time between two `manage.rows` frames. While a step runs
    /// the runner records its progress several times a second, and every
    /// frame is a refetch by every Manage card open; a row redrawn once a
    /// second is live enough.
    pub manage_rows_every: Duration,
    pub heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            debounce: Duration::from_millis(300),
            manage_rows_every: Duration::from_secs(1),
            heartbeat: HEARTBEAT,
        }
    }
}

/// The name, under `system/`, of the file the watch writes to hear its
/// own watcher deliver: an FSEvents stream starts asynchronously, so a
/// watch that has been set up is not yet one that reports.
const READY_MARKER: &str = ".watch-ready";

/// A dataset the UI fetches, named by what serves it. A card subscribes
/// to the ones it reads and refetches those; a change to anything else
/// never reaches it. The set is closed and mirrored by hand in
/// `ui/src/live.ts`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
pub enum Table {
    /// `GET /api/dag`: the loop's record, in `system/supervisor.sqlite`,
    /// written on every step state change, whoever runs the loop.
    #[serde(rename = "dag")]
    #[strum(serialize = "dag")]
    Dag,
    /// `GET /api/manage/rows`: the join over the config, the record,
    /// the run store's runs, steps, metrics and the run's own log
    /// lines, and the storage samples. Not the server's log lines: a
    /// row never reads those, and a refetch that logged a line would
    /// otherwise be the next refetch's cause.
    #[serde(rename = "manage.rows")]
    #[strum(serialize = "manage.rows")]
    ManageRows,
    /// `GET /api/runs`: which runs exist and which steps took part.
    #[serde(rename = "runs")]
    #[strum(serialize = "runs")]
    Runs,
    /// `GET /api/log` and a run's log: any line, from either writer.
    #[serde(rename = "log")]
    #[strum(serialize = "log")]
    Log,
    /// `GET /api/pipeline/storage`: the sampler walked the root.
    #[serde(rename = "storage")]
    #[strum(serialize = "storage")]
    Storage,
}

impl Table {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Something in the data root moved, or the stream is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootEvent {
    /// `<root>/config.toml` was written — by this server's own
    /// `PUT /api/config`, by an agent, or by hand in an editor. The
    /// datasets the config feeds are reported beside it as
    /// [`RootEvent::TableChanged`] frames.
    ConfigChanged,
    /// A dataset's inputs moved; fetch it again. Payload-free like the
    /// rest: every consumer already diffs what it fetches.
    TableChanged { table: Table },
    /// A component appeared, changed or vanished under
    /// `system/frontend/`.
    FrontendChanged,
    /// The grid index (`unified_index/grid_index/db.doltlite_db`) has a
    /// new HEAD. Under streaming that happens many times per sync -- a
    /// `grid_index` pass per checkpoint -- and it is how rows reach the
    /// grid while the download that produced them is still running. The
    /// commit, not the file: the applet reads at HEAD, so a write that
    /// has not committed is not something a reader can fetch yet.
    IndexChanged,
    /// Nothing changed; the stream is open. See [`HEARTBEAT`].
    Heartbeat,
}

/// A [`RootEvent`] as it goes out on the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct RootFrame {
    #[serde(flatten)]
    pub event: RootEvent,
    /// Set when a request's own effect is what moved: how many requests
    /// in a row have each caused the next. A page echoes it on the
    /// refetch it makes (`loop_guard`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<u32>,
}

impl From<RootEvent> for RootFrame {
    fn from(event: RootEvent) -> Self {
        RootFrame { event, chain: None }
    }
}

/// Fan-out channel for [`RootFrame`]s. Subscribed by
/// `GET /api/sync/stream`.
pub type RootTx = broadcast::Sender<RootFrame>;

/// A file the watcher reports on. What the filesystem can say; the
/// datasets it feeds are `expand`'s business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Moved {
    Config,
    /// The loop's record and its mailbox, `system/supervisor.sqlite`:
    /// heard by [`watch_record`], not seen by the filesystem.
    Supervisor,
    RunStore,
    Frontend,
    GridIndex,
    /// The watch's own marker: it delivers.
    Ready,
}

fn classify(root: &Path, path: &Path) -> Option<Moved> {
    // The atomic-write temp files are the same change reported twice;
    // the rename that follows is the one worth reporting.
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".tmp") || datalib_core::disk::is_store_lock(name) {
        return None;
    }
    if path == root.join("config.toml") {
        return Some(Moved::Config);
    }
    let system = root.join("system");
    if path.parent() == Some(system.as_path()) && name.starts_with(READY_MARKER) {
        return Some(Moved::Ready);
    }
    let frontend = system.join("frontend");
    // A component lives under the directory; the directory itself
    // appearing is not one. `spawn` creates it just before arming the
    // watch, and macOS reports that creation once the stream is live.
    if path.starts_with(&frontend) && path != frontend {
        return Some(Moved::Frontend);
    }
    // `runs.sqlite-wal` / `-journal` are the same write as the
    // database itself, so match on the stem rather than equality.
    if path.parent() == Some(root.join(datalib_core::layout::RUNS_DIR_REL).as_path())
        && name.starts_with("runs.sqlite")
    {
        return Some(Moved::RunStore);
    }
    if path.parent() == Some(datalib_core::layout::grid_index_dir(root).as_path())
        && name.starts_with(datalib_core::layout::GRID_DB)
    {
        return Some(Moved::GridIndex);
    }
    None
}

/// The datasets a part of the run store feeds.
fn tables_of(part: StorePart) -> &'static [Table] {
    match part {
        StorePart::Runs | StorePart::StepRuns => &[Table::Runs, Table::ManageRows],
        StorePart::Metrics => &[Table::ManageRows],
        StorePart::RunLog => &[Table::Log, Table::ManageRows],
        StorePart::ProcessLog => &[Table::Log],
    }
}

/// Which parts of the run store moved since the last look, by their
/// write counters. `seen` is updated in place. A store that was
/// replaced — a schema bump remakes the file — reads as every part
/// moving, which is right: everything a reader held is gone.
fn moved_parts(
    now: &BTreeMap<StorePart, i64>,
    seen: &mut BTreeMap<StorePart, i64>,
) -> Vec<StorePart> {
    let moved = now
        .iter()
        .filter(|(part, v)| seen.get(part) != Some(v))
        .map(|(part, _)| *part)
        .collect();
    *seen = now.clone();
    moved
}

/// What the watcher remembers between bursts, so a burst reports what
/// moved and not what a file already held.
struct Seen {
    /// The run store's write counters as of the last burst.
    runs: BTreeMap<StorePart, i64>,
    /// The grid index's HEAD as of the last burst.
    index_head: Option<String>,
    /// The last log line read for its chain.
    log_seq: i64,
}

impl Seen {
    async fn now(root: &Path) -> Seen {
        Seen {
            runs: datalib_runs::versions(root).await,
            index_head: index_head(root).await.unwrap_or(None),
            log_seq: datalib_runs::last_log_seq(root).await,
        }
    }
}

/// More server lines than this in one burst is not a page echoing
/// itself, and not worth reading to find out.
const CHAIN_READ_LIMIT: i64 = 500;

/// The chain the server's own new lines continue, and the cursor moved
/// past them.
async fn server_log_chain(root: &Path, seen: &mut Seen) -> Option<u32> {
    let lines = datalib_runs::process_log_after(root, seen.log_seq, CHAIN_READ_LIMIT).await;
    if lines.len() as i64 >= CHAIN_READ_LIMIT {
        seen.log_seq = datalib_runs::last_log_seq(root).await;
        return None;
    }
    if let Some(last) = lines.last() {
        seen.log_seq = last.seq;
    }
    crate::loop_guard::burst_chain(&lines)
}

/// A burst's frames. What moved outside the server's own log goes out
/// plain. What only the server's log moved goes out carrying `chain` —
/// a frame some other write would have sent anyway was not caused by
/// any request, so it starts nothing.
fn frames(
    elsewhere: HashSet<RootEvent>,
    server_log_only: HashSet<RootEvent>,
    chain: Option<u32>,
) -> HashSet<RootFrame> {
    let caused: Vec<RootFrame> = server_log_only
        .into_iter()
        .filter(|e| !elsewhere.contains(e))
        .map(|event| RootFrame { event, chain })
        .collect();
    elsewhere
        .into_iter()
        .map(RootFrame::from)
        .chain(caused)
        .collect()
}

/// The least time between two frames of this event, when it has one.
fn min_interval(event: RootEvent, timing: &Timing) -> Option<Duration> {
    match event {
        RootEvent::TableChanged {
            table: Table::ManageRows,
        } => Some(timing.manage_rows_every),
        _ => None,
    }
}

/// Holds back a frame that comes sooner than [`min_interval`] after the
/// last one of its event, and lets it out when that interval is up. The
/// first frame after a quiet spell goes at once; a stream of them goes
/// once per interval, the last one included, so nothing is lost.
#[derive(Default)]
struct Throttle {
    timing: Timing,
    last_sent: HashMap<RootEvent, Instant>,
    /// Held frames' events, with the chain they would carry.
    held: HashMap<RootEvent, Option<u32>>,
}

impl Throttle {
    /// `Some` to send now; `None` when held.
    fn offer(&mut self, frame: RootFrame, now: Instant) -> Option<RootFrame> {
        let Some(every) = min_interval(frame.event, &self.timing) else {
            return Some(frame);
        };
        let chain = match self.held.remove(&frame.event) {
            Some(held) => merge_chain(held, frame.chain),
            None => frame.chain,
        };
        let open = self
            .last_sent
            .get(&frame.event)
            .is_none_or(|&at| now >= at + every);
        if open {
            self.last_sent.insert(frame.event, now);
            Some(RootFrame {
                event: frame.event,
                chain,
            })
        } else {
            self.held.insert(frame.event, chain);
            None
        }
    }

    /// The held frames whose interval is up by `now`.
    fn due(&mut self, now: Instant) -> Vec<RootFrame> {
        let ready: Vec<RootEvent> = self
            .held
            .keys()
            .filter(|e| self.release_at(**e).is_some_and(|at| now >= at))
            .copied()
            .collect();
        ready
            .into_iter()
            .map(|event| {
                let chain = self.held.remove(&event).flatten();
                self.last_sent.insert(event, now);
                RootFrame { event, chain }
            })
            .collect()
    }

    /// When the next held frame is due; `None` when nothing is held.
    fn next_due(&self) -> Option<Instant> {
        self.held.keys().filter_map(|e| self.release_at(*e)).min()
    }

    fn release_at(&self, event: RootEvent) -> Option<Instant> {
        Some(*self.last_sent.get(&event)? + min_interval(event, &self.timing)?)
    }
}

/// Two frames of one event folded into one. A frame with no chain is one
/// something other than a request would have sent anyway, so it wins.
fn merge_chain(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    Some(a?.max(b?))
}

/// The grid index's HEAD, or `None` when there is no store or nothing
/// committed in it; `Err` when there is a store and its head could not be
/// read, which says nothing about whether it moved. Opened read-only for
/// the look and closed again: a handle held across a rebuild would point
/// at a file that is gone. A read-only open beside the live `grid_index`
/// writer is measured safe by `doltlite_two_process_test`.
async fn index_head(root: &Path) -> anyhow::Result<Option<String>> {
    let path = datalib_core::layout::grid_index_db(root);
    if !path.exists() {
        return Ok(None);
    }
    let pool = datalib_pin::open_reader(&path).await?;
    let head = datalib_pin::head(&pool).await;
    pool.close().await;
    Ok(head?.map(|pin| pin.commit().to_string()))
}

/// The frames one debounced burst of file moves becomes.
async fn expand(root: &Path, moved: &HashSet<Moved>, seen: &mut Seen) -> HashSet<RootFrame> {
    let mut out = HashSet::new();
    let mut server_log = HashSet::new();
    let table = |t: Table| RootEvent::TableChanged { table: t };
    for m in moved {
        match m {
            Moved::Config => {
                out.insert(RootEvent::ConfigChanged);
                out.insert(table(Table::Dag));
                out.insert(table(Table::ManageRows));
            }
            Moved::Supervisor => {
                out.insert(table(Table::Dag));
                out.insert(table(Table::ManageRows));
            }
            Moved::RunStore => {
                for part in moved_parts(&datalib_runs::versions(root).await, &mut seen.runs) {
                    let into = if part == StorePart::ProcessLog {
                        &mut server_log
                    } else {
                        &mut out
                    };
                    into.extend(tables_of(part).iter().map(|t| table(*t)));
                }
            }
            Moved::Frontend => {
                out.insert(RootEvent::FrontendChanged);
            }
            Moved::GridIndex => match index_head(root).await {
                Ok(head) if head != seen.index_head => {
                    seen.index_head = head;
                    out.insert(RootEvent::IndexChanged);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("watch: the grid index's head is unreadable now: {e:#}"),
            },
            Moved::Ready => {}
        }
    }
    let chain = if server_log.is_empty() {
        None
    } else {
        server_log_chain(root, seen).await
    };
    frames(out, server_log, chain)
}

/// Resolves once the watch reports: its watcher has delivered its own
/// marker and the loop's record is being listened to. Never, if the
/// watcher could not be made (which it logs).
pub struct Ready(tokio::sync::oneshot::Receiver<()>);

impl Ready {
    pub async fn wait(self) {
        if self.0.await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub fn spawn(root: PathBuf, tx: RootTx) -> Ready {
    spawn_with(root, tx, Timing::default())
}

pub fn spawn_with(root: PathBuf, tx: RootTx, timing: Timing) -> Ready {
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    let heartbeat_tx = tx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(timing.heartbeat);
        // The first tick fires immediately; a subscriber that just
        // connected does not need to be told the stream is alive.
        tick.tick().await;
        loop {
            tick.tick().await;
            // `Err` means nobody is subscribed, which is the normal
            // state of a server with no browser attached.
            let _ = heartbeat_tx.send(RootEvent::Heartbeat.into());
        }
    });

    // Create the directories before watching them: a watch on a path
    // that does not exist yet is simply not registered, and `system/`
    // is absent on a data root that has never synced.
    let system = root.join("system");
    let _ = std::fs::create_dir_all(&system);
    let frontend = system.join("frontend");
    let _ = std::fs::create_dir_all(&frontend);
    let runs = root.join(datalib_core::layout::RUNS_DIR_REL);
    let _ = std::fs::create_dir_all(&runs);

    // Resolve symlinks once, and classify against the resolved form.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let system = std::fs::canonicalize(&system).unwrap_or(system);
    let frontend = std::fs::canonicalize(&frontend).unwrap_or(frontend);
    let runs = std::fs::canonicalize(&runs).unwrap_or(runs);
    // Not created here: `unified_index/` belongs to the steps and the
    // applet, and a root that has never synced has none. Watched once it
    // exists — see the debounce loop below.
    let grid_index = datalib_core::layout::grid_index_dir(&root);

    // notify calls back on its own thread, so hand off through an
    // unbounded channel rather than doing any work there.
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<Moved>();
    let (listening_tx, listening) = tokio::sync::oneshot::channel();
    tokio::spawn(watch_record(root.clone(), raw_tx.clone(), listening_tx));
    let listeners = datalib_dag::supervisor::announce::listeners_dir(&root);
    let make_watcher = {
        let (root, raw_tx) = (root.clone(), raw_tx.clone());
        move || watcher_for(root.clone(), listeners.clone(), raw_tx.clone())
    };
    let mut watcher = match make_watcher() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "watch: could not create a filesystem watcher ({e}); \
                 the UI will not see external changes to this root"
            );
            return Ready(ready);
        }
    };

    // Three watches rather than one recursive watch on the root: the
    // root *is* the data mirror, so a recursive watch would follow
    // every blob a sync writes — thousands of events describing files
    // no UI surface reads.
    for (dir, mode) in [
        (root.as_path(), RecursiveMode::NonRecursive),
        (system.as_path(), RecursiveMode::NonRecursive),
        (runs.as_path(), RecursiveMode::NonRecursive),
        (frontend.as_path(), RecursiveMode::Recursive),
    ] {
        if let Err(e) = watcher.watch(dir, mode) {
            tracing::warn!("watch: {} ({e})", dir.display());
        }
    }
    let mut index_watcher = watch_index(&grid_index, &make_watcher);
    let marker = system.join(format!("{READY_MARKER}-{}", std::process::id()));

    tokio::spawn(async move {
        // The debounce task owns the watcher, because dropping a
        // watcher stops the watch and there is nowhere better to put
        // it: `AppState` is cloned per request. The task cannot end —
        // the only sender lives in the watcher's callback, which this
        // task now holds — so the watch lasts as long as the process,
        // which is exactly its intended lifetime.
        let _watcher = watcher;
        // Started from the stores so the first burst reports only what
        // moved in them, not everything they already held.
        let mut seen = Seen::now(&root).await;
        let mut throttle = Throttle {
            timing,
            ..Default::default()
        };
        // Written once the stores are read, so whatever moves after the
        // marker is heard is a move from what `seen` holds.
        let mut ready_tx = Some(ready_tx);
        let mut listening = Some(listening);
        let _ = std::fs::write(&marker, "");
        loop {
            // Wait for a file to move, or for a held frame to come due.
            let first = match throttle.next_due() {
                Some(at) => tokio::select! {
                    moved = raw_rx.recv() => moved,
                    _ = tokio::time::sleep_until(at) => {
                        for frame in throttle.due(Instant::now()) {
                            let _ = tx.send(frame);
                        }
                        continue;
                    }
                },
                None => raw_rx.recv().await,
            };
            let Some(first) = first else {
                return;
            };
            // Open a window on the first event, then coalesce
            // everything that lands inside it. One burst → one message
            // per kind.
            let mut pending = HashSet::from([first]);
            let deadline = tokio::time::Instant::now() + timing.debounce;
            // Ends on the window closing (`Err`) or the sender going
            // away (`Ok(None)`) — both mean "publish what you have".
            while let Ok(Some(moved)) = tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                pending.insert(moved);
            }
            // The grid index's directory may not exist at boot. Looked for
            // on every burst, and watched by a watcher of its own once it
            // does: on macOS every `watch` call restarts the watcher's
            // stream, and a restarted stream loses what lands meanwhile.
            if index_watcher.is_none() {
                index_watcher = watch_index(&grid_index, &make_watcher);
            }
            if pending.remove(&Moved::Ready) {
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = std::fs::remove_file(&marker);
                    if let Some(listening) = listening.take() {
                        let _ = listening.await;
                    }
                    let _ = ready_tx.send(());
                }
            }
            let now = Instant::now();
            let fresh = expand(&root, &pending, &mut seen).await;
            let mut sent: Vec<RootFrame> = fresh
                .into_iter()
                .filter_map(|f| throttle.offer(f, now))
                .collect();
            sent.extend(throttle.due(now));
            for frame in sent {
                let _ = tx.send(frame);
            }
        }
    });
    Ready(ready)
}

/// A watcher reporting what [`classify`] names under `root`. A config
/// change is also announced to the loop, which re-reads its config when
/// told to.
fn watcher_for(
    root: PathBuf,
    listeners: PathBuf,
    raw_tx: tokio::sync::mpsc::UnboundedSender<Moved>,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        // Reading something is not changing it, and on Linux this is not a
        // nicety — it is the difference between a push channel and a
        // feedback loop.
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        for path in &ev.paths {
            if let Some(moved) = classify(&root, path) {
                if moved == Moved::Config {
                    use datalib_dag::supervisor::announce::{
                        announce, CONFIG_CHANGED, FROM_SERVER,
                    };
                    announce(&listeners, FROM_SERVER, CONFIG_CHANGED);
                }
                let _ = raw_tx.send(moved);
            }
        }
    })
}

/// A watcher on the grid index's directory, once there is one.
fn watch_index(
    dir: &Path,
    make: &impl Fn() -> notify::Result<notify::RecommendedWatcher>,
) -> Option<notify::RecommendedWatcher> {
    if !dir.is_dir() {
        return None;
    }
    let mut watcher = make().ok()?;
    watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

/// The loop's record moves on a commit, and every commit to it is
/// announced; the filesystem would report writes, and none at all to a
/// file already held open. The lock and the config have announcements of
/// their own that are not the record moving.
async fn watch_record(
    root: PathBuf,
    moved: tokio::sync::mpsc::UnboundedSender<Moved>,
    listening: tokio::sync::oneshot::Sender<()>,
) {
    use datalib_dag::supervisor::announce::{Listener, CONFIG_CHANGED, RUNNER_LOCK_RELEASED};
    let store = match datalib_dag::supervisor::store::Store::open(&root).await {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "watch: cannot open the loop's record, so the UI will not see it move: {e:#}"
            );
            return;
        }
    };
    let mut listener = Listener::new(&store, "the UI's watch").await;
    let _ = listening.send(());
    loop {
        let heard = listener.next(&store).await;
        let record = heard
            .iter()
            .any(|line| line != RUNNER_LOCK_RELEASED && line != CONFIG_CHANGED);
        if record && moved.send(Moved::Supervisor).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_and_the_runner_record_are_told_apart() {
        let root = Path::new("/data");
        assert_eq!(
            classify(root, &root.join("config.toml")),
            Some(Moved::Config)
        );
        assert_eq!(
            classify(root, &root.join("system/runs/runs.sqlite-wal")),
            Some(Moved::RunStore)
        );
        assert_eq!(
            classify(root, &root.join("system/frontend/user/abc.js")),
            Some(Moved::Frontend)
        );
        assert_eq!(classify(root, &root.join("system/frontend")), None);
        // A writer taking or releasing its lock is not the index moving.
        assert_eq!(
            classify(
                root,
                &root.join("unified_index/grid_index/db.doltlite_db.lock")
            ),
            None
        );
        assert_eq!(
            classify(root, &root.join("unified_index/grid_index/db.doltlite_db")),
            Some(Moved::GridIndex)
        );
    }

    /// A frame the server's own lines alone moved carries the chain; one
    /// a sync's write would have sent anyway carries none, or every live
    /// card would count as looping for as long as a sync runs.
    #[test]
    fn only_a_frame_the_server_log_alone_moved_carries_a_chain() {
        let log = RootEvent::TableChanged { table: Table::Log };
        let runs = RootEvent::TableChanged { table: Table::Runs };
        let alone = frames(HashSet::new(), HashSet::from([log]), Some(3));
        assert_eq!(
            alone,
            HashSet::from([RootFrame {
                event: log,
                chain: Some(3)
            }])
        );
        let with_a_run = frames(HashSet::from([log, runs]), HashSet::from([log]), Some(3));
        assert_eq!(
            with_a_run,
            HashSet::from([RootFrame::from(log), RootFrame::from(runs)])
        );
    }

    /// A sync's progress moves the run store every few hundred
    /// milliseconds; without the throttle every Manage card open refetched
    /// its rows on each move, and each refetch was a line in the log.
    #[test]
    fn manage_rows_frames_go_once_a_second_and_the_last_is_not_lost() {
        let rows = RootFrame::from(RootEvent::TableChanged {
            table: Table::ManageRows,
        });
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut throttle = Throttle::default();

        assert_eq!(
            throttle.offer(rows, ms(0)),
            Some(rows),
            "the first goes at once"
        );
        assert_eq!(throttle.offer(rows, ms(300)), None);
        assert_eq!(throttle.offer(rows, ms(600)), None);
        assert_eq!(throttle.next_due(), Some(ms(1_000)));
        assert!(throttle.due(ms(999)).is_empty());
        assert_eq!(throttle.due(ms(1_000)), [rows], "held ones go as one");
        assert_eq!(throttle.next_due(), None);

        assert_eq!(
            throttle.offer(rows, ms(1_300)),
            None,
            "a second after the last sent"
        );
        assert_eq!(
            throttle.offer(rows, ms(2_000)),
            Some(rows),
            "folds in the held one"
        );
        assert!(throttle.due(ms(5_000)).is_empty());
    }

    #[test]
    fn other_frames_are_not_throttled() {
        let dag = RootFrame::from(RootEvent::TableChanged { table: Table::Dag });
        let t0 = Instant::now();
        let mut throttle = Throttle::default();
        assert_eq!(throttle.offer(dag, t0), Some(dag));
        assert_eq!(throttle.offer(dag, t0), Some(dag));
        assert_eq!(throttle.next_due(), None);
    }

    #[test]
    fn a_held_frame_keeps_a_chain_only_if_every_frame_it_folds_had_one() {
        let rows = RootEvent::TableChanged {
            table: Table::ManageRows,
        };
        let t0 = Instant::now();
        let every = Timing::default().manage_rows_every;
        let later = t0 + every;
        let mut throttle = Throttle::default();
        throttle.offer(rows.into(), t0);
        let chained = |c| RootFrame {
            event: rows,
            chain: Some(c),
        };
        throttle.offer(chained(2), t0);
        throttle.offer(chained(4), t0);
        assert_eq!(throttle.due(later), [chained(4)]);

        let t1 = later + every / 2;
        throttle.offer(chained(3), t1);
        throttle.offer(rows.into(), t1);
        assert_eq!(throttle.due(later + every), [RootFrame::from(rows)]);
    }

    /// `ui/src/live.ts` reads `chain` beside `kind`, and a frame with
    /// none looks as it always did.
    #[test]
    fn a_frame_is_its_event_plus_an_optional_chain() {
        let log = RootEvent::TableChanged { table: Table::Log };
        assert_eq!(
            serde_json::to_string(&RootFrame::from(log)).unwrap(),
            r#"{"kind":"table_changed","table":"log"}"#
        );
        assert_eq!(
            serde_json::to_string(&RootFrame {
                event: log,
                chain: Some(2)
            })
            .unwrap(),
            r#"{"kind":"table_changed","table":"log","chain":2}"#
        );
    }

    /// The wire spelling is what `ui/src/live.ts` switches on.
    #[test]
    fn table_names_agree_between_strum_and_serde() {
        use strum::VariantArray;
        for &t in Table::VARIANTS {
            assert_eq!(
                serde_json::to_string(&t).unwrap(),
                format!("\"{}\"", t.as_str())
            );
        }
        assert_eq!(
            serde_json::to_string(&RootEvent::TableChanged {
                table: Table::ManageRows
            })
            .unwrap(),
            r#"{"kind":"table_changed","table":"manage.rows"}"#
        );
    }

    /// The point of asking the store which part moved: the server's own
    /// log lines wake the log grid and nothing else. Without this every
    /// refetch of the Manage rows that logged a line would be the cause
    /// of the next one.
    #[test]
    fn a_server_log_line_wakes_the_log_and_not_the_manage_rows() {
        let mut seen = BTreeMap::from([(StorePart::ProcessLog, 3), (StorePart::Metrics, 7)]);
        let now = BTreeMap::from([(StorePart::ProcessLog, 4), (StorePart::Metrics, 7)]);
        let tables: HashSet<Table> = moved_parts(&now, &mut seen)
            .into_iter()
            .flat_map(|p| tables_of(p).iter().copied())
            .collect();
        assert_eq!(tables, HashSet::from([Table::Log]));
        assert_eq!(seen, now, "the look is remembered");

        // A run's line is both a log line and an input to the rows.
        let now = BTreeMap::from([(StorePart::ProcessLog, 4), (StorePart::RunLog, 1)]);
        let tables: HashSet<Table> = moved_parts(&now, &mut seen)
            .into_iter()
            .flat_map(|p| tables_of(p).iter().copied())
            .collect();
        assert_eq!(tables, HashSet::from([Table::Log, Table::ManageRows]));
    }

    /// The reason the directory watch filters by name at all: `system/`
    /// holds stores no frame is about, and reporting their writes as
    /// `DagChanged` would make every reader refetch the loop's record
    /// for nothing.
    #[test]
    fn the_stores_beside_the_runner_record_are_not_the_runner_record() {
        let root = Path::new("/data");
        for quiet in [
            "system/feedback.doltlite_db",
            "system/usage.doltlite_db",
            "system/api-token",
            "system/supervisor.sqlite",
            "system/supervisor.sqlite-wal",
            "slack/raw/blobs.doltlite_db",
            "config.yaml",
        ] {
            assert_eq!(classify(root, &root.join(quiet)), None, "{quiet}");
        }
    }

    /// An atomic write is `tmp` + `rename`. Reporting the temp file
    /// doubles every change, and — worse — reports it while the file
    /// is still half-written, so a client that refetched on it would
    /// race the rename.
    #[test]
    fn the_temp_half_of_an_atomic_write_is_not_a_change() {
        let root = Path::new("/data");
        assert_eq!(classify(root, &root.join("config.tmp")), None);
    }

    /// A watch's clocks at zero, so a test waits on nothing but what it
    /// asserts.
    fn at_once() -> Timing {
        Timing {
            debounce: Duration::ZERO,
            manage_rows_every: Duration::ZERO,
            heartbeat: Duration::from_secs(3600),
        }
    }

    async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), f)
            .await
            .unwrap_or_else(|_| panic!("no {what} within 10s"))
    }

    /// A watch on `root` that has said it reports.
    async fn watching(root: &Path) -> broadcast::Receiver<RootFrame> {
        let (tx, rx) = broadcast::channel(1024);
        let ready = spawn_with(root.to_path_buf(), tx, at_once());
        within("readiness", ready.wait()).await;
        rx
    }

    /// The events reported up to and including the first `want`.
    async fn until(rx: &mut broadcast::Receiver<RootFrame>, want: RootEvent) -> Vec<RootEvent> {
        within(&format!("{want:?}"), async {
            let mut got = Vec::new();
            loop {
                let event = rx
                    .recv()
                    .await
                    .expect("the channel neither lags nor closes")
                    .event;
                got.push(event);
                if event == want {
                    return got;
                }
            }
        })
        .await
    }

    /// What the watch reported before a write of the test's own under
    /// `system/frontend/`. The watch takes events in the order they
    /// happened, so whatever an earlier write would have reported has
    /// been by the time the barrier's `FrontendChanged` arrives; a burst
    /// is sent whole, so what came with it is read too. Once per test: the
    /// barrier's write may be reported more than once.
    async fn barrier(root: &Path, rx: &mut broadcast::Receiver<RootFrame>) -> Vec<RootEvent> {
        std::fs::write(root.join("system/frontend/barrier.js"), "").unwrap();
        let mut got = until(rx, RootEvent::FrontendChanged).await;
        while let Ok(frame) = rx.try_recv() {
            got.push(frame.event);
        }
        got.retain(|e| *e != RootEvent::FrontendChanged);
        got
    }

    /// The end-to-end claim this module exists to make: a write to
    /// `config.toml` by *someone else* — an agent, an editor, a
    /// `datalib-migrate-config` — reaches a subscriber without anyone
    /// having asked, and reaches the loop, which re-reads its config when
    /// it hears so.
    #[tokio::test]
    async fn an_external_config_write_reaches_a_subscriber_and_the_loop() {
        use datalib_dag::supervisor::announce::{Listener, CONFIG_CHANGED};
        let td = tempfile::tempdir().unwrap();
        let store = datalib_dag::supervisor::store::Store::open(td.path())
            .await
            .unwrap();
        let mut the_loop = Listener::new(&store, "test").await;
        let mut rx = watching(td.path()).await;

        let tmp = td.path().join("config.tmp");
        std::fs::write(&tmp, "# rewritten\n").unwrap();
        std::fs::rename(&tmp, td.path().join("config.toml")).unwrap();
        until(&mut rx, RootEvent::ConfigChanged).await;
        let heard = within("the announcement", the_loop.next(&store)).await;
        assert!(heard.iter().any(|l| l == CONFIG_CHANGED), "{heard:?}");
    }

    /// The same for the loop's record, whoever runs the loop. The store is
    /// opened before the watch starts, as a running loop's is: its file
    /// appearing once is not what a subscriber needs to hear, its every
    /// commit is.
    #[tokio::test]
    async fn a_terminal_loops_record_write_reaches_a_subscriber() {
        let td = tempfile::tempdir().unwrap();
        let store = datalib_dag::supervisor::store::Store::open(td.path())
            .await
            .unwrap();
        let mut rx = watching(td.path()).await;
        store.pause("a/raw", "loop").await.unwrap();
        until(&mut rx, RootEvent::TableChanged { table: Table::Dag }).await;
    }

    /// A data root reached through a symlink still reports.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_root_behind_a_symlink_still_reports() {
        let td = tempfile::tempdir().unwrap();
        let real = td.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = td.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Watched through the link...
        let mut rx = watching(&link).await;
        // ...and written through the real path, the way another process
        // that resolved it would.
        let tmp = real.join("config.tmp");
        std::fs::write(&tmp, "# rewritten\n").unwrap();
        std::fs::rename(&tmp, real.join("config.toml")).unwrap();
        until(&mut rx, RootEvent::ConfigChanged).await;
    }

    /// Reading the component store is not a change to it: on Linux a read
    /// reported as a change is a feedback loop, not just a spurious
    /// refetch.
    #[tokio::test]
    async fn reading_the_component_store_is_not_a_change_to_it() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("system/frontend/c.js"), "x").ok();
        let mut rx = watching(td.path()).await;
        let frontend = td.path().join("system/frontend");
        for _ in 0..20 {
            // What `FrontendStore::scan` does: walk it and open what it
            // finds.
            for e in std::fs::read_dir(&frontend).unwrap().flatten() {
                let _ = std::fs::read(e.path());
            }
            let _ = std::fs::read(td.path().join("config.toml"));
        }
        assert_eq!(barrier(td.path(), &mut rx).await, []);
    }

    /// The server's own log line, written through the real writer,
    /// reaches a subscriber as the log dataset — and as nothing else.
    /// The negative half is the one that matters: a `manage.rows` here
    /// would be the Manage screen refetching on its own log lines.
    #[tokio::test]
    async fn a_server_log_line_reaches_the_log_and_nothing_else() {
        let td = tempfile::tempdir().unwrap();
        let mut rx = watching(td.path()).await;
        let server = datalib_runs::ProcessLogWriter::start(
            td.path(),
            datalib_runs::Process::Http,
            None,
            datalib_runs::Retention::default(),
        )
        .unwrap();
        server.log(datalib_runs::LogRow {
            level: "debug".into(),
            msg: "a line".into(),
            ..Default::default()
        });
        let log = RootEvent::TableChanged { table: Table::Log };
        let mut got = until(&mut rx, log).await;
        // Everything the writer will ever write is on disk once it is gone.
        drop(server);
        got.extend(barrier(td.path(), &mut rx).await);
        let rows = RootEvent::TableChanged {
            table: Table::ManageRows,
        };
        assert!(
            !got.contains(&rows),
            "a server log line was reported as a change to the Manage rows: {got:?}"
        );
    }

    /// A commit to the grid index, the way the `grid_index` step makes
    /// one: its own process, opening and closing the store around it.
    async fn write_index(db: &Path, id: i64, commit: bool) {
        let writer = datalib_core::store::open_pool(db).await.unwrap();
        sqlx::query("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)")
            .execute(&writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (?)")
            .bind(id)
            .execute(&writer)
            .await
            .unwrap();
        if commit {
            let hash: Option<String> = sqlx::query_scalar("SELECT dolt_commit('-Am', 'rows')")
                .fetch_one(&writer)
                .await
                .unwrap();
            hash.expect("doltlite linked");
        }
        writer.close().await;
    }

    /// The grid index reports when its HEAD moves and not when its file
    /// does. The step writes the file throughout a pass — the working set
    /// lives in it — and commits once at the end; a grid told to refetch
    /// on the writes would fetch the same HEAD again each time, and be
    /// told nothing when the rows it can read actually changed.
    #[tokio::test]
    async fn the_grid_index_reports_its_commits_and_not_its_writes() {
        let td = tempfile::tempdir().unwrap();
        // The applet creates the directory at boot, before the first
        // pass; the watch is armed on it from the start here too.
        let db = datalib_core::layout::grid_index_db(td.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let mut rx = watching(td.path()).await;

        write_index(&db, 1, true).await;
        until(&mut rx, RootEvent::IndexChanged).await;
        for i in 100..110 {
            write_index(&db, i, false).await;
        }
        let got = barrier(td.path(), &mut rx).await;
        assert!(
            !got.contains(&RootEvent::IndexChanged),
            "a write the step has not committed was reported as an index change: {got:?}"
        );
        write_index(&db, 200, true).await;
        until(&mut rx, RootEvent::IndexChanged).await;
    }

    /// A head that cannot be read says nothing about whether it moved.
    /// Taken for "no head", it read as a move, and again as one when it
    /// could be read, and the grid refetched twice for nothing.
    #[tokio::test]
    async fn an_unreadable_index_head_is_not_a_move() {
        let td = tempfile::tempdir().unwrap();
        let db = datalib_core::layout::grid_index_db(td.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let mut rx = watching(td.path()).await;
        write_index(&db, 1, true).await;
        until(&mut rx, RootEvent::IndexChanged).await;

        std::fs::write(&db, "not a store").unwrap();
        let got = barrier(td.path(), &mut rx).await;
        assert!(!got.contains(&RootEvent::IndexChanged), "{got:?}");
    }

    /// The control for the filter, and the reason `classify` is not
    /// simply "anything under `system/`".
    #[tokio::test]
    async fn writes_to_the_usage_store_are_not_reported() {
        let td = tempfile::tempdir().unwrap();
        let mut rx = watching(td.path()).await;
        let usage = td.path().join("system/usage.doltlite_db");
        for n in 0..20 {
            std::fs::write(&usage, format!("row {n}")).unwrap();
        }
        assert_eq!(barrier(td.path(), &mut rx).await, []);
    }

    /// The watch's own marker is how it knows it reports, and is never
    /// reported itself.
    #[test]
    fn the_readiness_marker_is_the_watchs_own() {
        let root = Path::new("/data");
        assert_eq!(
            classify(root, &root.join("system/.watch-ready-42")),
            Some(Moved::Ready)
        );
        assert_eq!(classify(root, &root.join(".watch-ready-42")), None);
    }
}
