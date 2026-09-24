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

use datalib_dag::supervisor::wake::Listener;
use datalib_runs::StorePart;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// How long to hold a burst of filesystem events before publishing.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// The least time between two `manage.rows` frames. While a step runs
/// the runner records its progress several times a second, and every
/// frame is a refetch by every Manage card open; a row redrawn once a
/// second is live enough.
const MANAGE_ROWS_EVERY: Duration = Duration::from_secs(1);

/// How often to publish a [`RootEvent::Heartbeat`] on an otherwise
/// silent stream.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

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
    /// seen by [`watch_record`], not by the filesystem.
    Supervisor,
    RunStore,
    Frontend,
    GridIndex,
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
            index_head: index_head(root).await,
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
fn min_interval(event: RootEvent) -> Option<Duration> {
    match event {
        RootEvent::TableChanged {
            table: Table::ManageRows,
        } => Some(MANAGE_ROWS_EVERY),
        _ => None,
    }
}

/// Holds back a frame that comes sooner than [`min_interval`] after the
/// last one of its event, and lets it out when that interval is up. The
/// first frame after a quiet spell goes at once; a stream of them goes
/// once per interval, the last one included, so nothing is lost.
#[derive(Default)]
struct Throttle {
    last_sent: HashMap<RootEvent, Instant>,
    /// Held frames' events, with the chain they would carry.
    held: HashMap<RootEvent, Option<u32>>,
}

impl Throttle {
    /// `Some` to send now; `None` when held.
    fn offer(&mut self, frame: RootFrame, now: Instant) -> Option<RootFrame> {
        let Some(every) = min_interval(frame.event) else {
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
        Some(*self.last_sent.get(&event)? + min_interval(event)?)
    }
}

/// Two frames of one event folded into one. A frame with no chain is one
/// something other than a request would have sent anyway, so it wins.
fn merge_chain(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    Some(a?.max(b?))
}

/// The grid index's HEAD, or `None` when there is no store or nothing
/// committed in it. Opened read-only for the look and closed again: a
/// handle held across a rebuild would point at a file that is gone. A
/// read-only open beside the live `grid_index` writer is measured safe
/// by `doltlite_two_process_test`.
async fn index_head(root: &Path) -> Option<String> {
    let path = datalib_core::layout::grid_index_db(root);
    let pool = datalib_pin::open_reader(&path).await.ok()?;
    let head = datalib_pin::head(&pool).await.ok().flatten();
    pool.close().await;
    head.map(|pin| pin.commit().to_string())
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
            Moved::GridIndex => {
                let head = index_head(root).await;
                if head != seen.index_head {
                    seen.index_head = head;
                    out.insert(RootEvent::IndexChanged);
                }
            }
        }
    }
    let chain = if server_log.is_empty() {
        None
    } else {
        server_log_chain(root, seen).await
    };
    frames(out, server_log, chain)
}

pub fn spawn(root: PathBuf, tx: RootTx) {
    let heartbeat_tx = tx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT);
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
    tokio::spawn(watch_record(root.clone(), raw_tx.clone()));
    let watch_root = root.clone();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            // Reading something is not changing it, and on Linux this is
            // not a nicety — it is the difference between a push channel
            // and a feedback loop.
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for path in &ev.paths {
                if let Some(moved) = classify(&watch_root, path) {
                    let _ = raw_tx.send(moved);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    "watch: could not create a filesystem watcher ({e}); \
                 the UI will not see external changes to this root"
                );
                return;
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
    let mut index_watched = watcher
        .watch(&grid_index, RecursiveMode::NonRecursive)
        .is_ok();

    tokio::spawn(async move {
        // The debounce task owns the watcher, because dropping a
        // watcher stops the watch and there is nowhere better to put
        // it: `AppState` is cloned per request. The task cannot end —
        // the only sender lives in the watcher's callback, which this
        // task now holds — so the watch lasts as long as the process,
        // which is exactly its intended lifetime.
        let mut watcher = watcher;
        // Started from the stores so the first burst reports only what
        // moved in them, not everything they already held.
        let mut seen = Seen::now(&root).await;
        let mut throttle = Throttle::default();
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
            let deadline = tokio::time::Instant::now() + DEBOUNCE;
            // Ends on the window closing (`Err`) or the sender going
            // away (`Ok(None)`) — both mean "publish what you have".
            while let Ok(Some(moved)) = tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                pending.insert(moved);
            }
            // The grid index's directory may not exist at boot, and a
            // watch on a path that did not exist was never registered.
            // Try again on every burst until it takes: a `watch` call on
            // an absent path is cheap, and the alternative is a first
            // pass nobody hears about.
            if !index_watched {
                index_watched = watcher
                    .watch(&grid_index, RecursiveMode::NonRecursive)
                    .is_ok();
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
}

/// The loop's record is not a file whose writes FSEvents reports: the
/// loop holds its connection open, and FSEvents does not announce a write
/// to a file held open. `wake::Listener` watches it with kqueue (inotify
/// on Linux) instead, and `PRAGMA data_version` on a connection of our own
/// says whether anything committed moved.
async fn watch_record(root: PathBuf, moved: tokio::sync::mpsc::UnboundedSender<Moved>) {
    let store = match datalib_dag::supervisor::store::Store::open(&root).await {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "watch: cannot open the loop's record, so the UI will not see it move: {e:#}"
            );
            return;
        }
    };
    let mut listener = Listener::new(&store, "watch", &[]).await;
    let mut seen = None;
    loop {
        match store.data_version().await {
            Ok(version) => {
                if seen.is_some_and(|s| s != version) && moved.send(Moved::Supervisor).is_err() {
                    return;
                }
                seen = Some(version);
            }
            Err(e) => tracing::warn!("watch: could not read the record's version: {e:#}"),
        }
        listener.next(&store).await;
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
        let later = t0 + MANAGE_ROWS_EVERY;
        let mut throttle = Throttle::default();
        throttle.offer(rows.into(), t0);
        let chained = |c| RootFrame {
            event: rows,
            chain: Some(c),
        };
        throttle.offer(chained(2), t0);
        throttle.offer(chained(4), t0);
        assert_eq!(throttle.due(later), [chained(4)]);

        let t1 = later + MANAGE_ROWS_EVERY / 2;
        throttle.offer(chained(3), t1);
        throttle.offer(rows.into(), t1);
        assert_eq!(
            throttle.due(later + MANAGE_ROWS_EVERY),
            [RootFrame::from(rows)]
        );
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

    async fn heard(
        rx: &mut broadcast::Receiver<RootFrame>,
        want: RootEvent,
        mut stimulus: impl FnMut(),
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            stimulus();
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Ok(got)) if got.event == want => return,
                // Some other kind, or a lagged receiver: keep listening.
                Ok(_) => continue,
                Err(_) if tokio::time::Instant::now() < deadline => continue,
                Err(_) => panic!("no {want:?} within 20s"),
            }
        }
    }

    /// The end-to-end claim this module exists to make: a write to
    /// `config.toml` by *someone else* — an agent, an editor, a
    /// `datalib-migrate-config` — reaches a subscriber without anyone
    /// having asked.
    #[tokio::test]
    async fn an_external_config_write_reaches_a_subscriber() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let root = td.path().to_path_buf();
        let mut n = 0;
        heard(&mut rx, RootEvent::ConfigChanged, move || {
            n += 1;
            let tmp = root.join("config.tmp");
            std::fs::write(&tmp, format!("# rewrite {n}\n")).unwrap();
            std::fs::rename(&tmp, root.join("config.toml")).unwrap();
        })
        .await;
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
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let writer = tokio::spawn(async move {
            loop {
                store.pause("a/raw", "loop").await.unwrap();
                store.resume("a/raw").await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        heard(
            &mut rx,
            RootEvent::TableChanged { table: Table::Dag },
            || {},
        )
        .await;
        writer.abort();
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

        let (tx, mut rx) = broadcast::channel(64);
        // Watched through the link...
        spawn(link, tx);

        // ...and written through the real path, the way another
        // process that resolved it would.
        let mut n = 0;
        heard(&mut rx, RootEvent::ConfigChanged, move || {
            n += 1;
            let tmp = real.join("config.tmp");
            std::fs::write(&tmp, format!("# rewrite {n}\n")).unwrap();
            std::fs::rename(&tmp, real.join("config.toml")).unwrap();
        })
        .await;
    }

    /// Reading the component store is not a change to it.
    #[tokio::test]
    async fn reading_the_component_store_is_not_a_change_to_it() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let frontend = td.path().join("system/frontend");
        for _ in 0..20 {
            // What `FrontendStore::scan` does: walk it and open what
            // it finds.
            let entries: Vec<_> = std::fs::read_dir(&frontend).unwrap().collect();
            for e in entries.into_iter().flatten() {
                let _ = std::fs::read(e.path());
            }
            let _ = std::fs::read(td.path().join("config.toml"));
        }
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            rx.try_recv().is_err(),
            "reading the data root was reported as changing it — on Linux \
             that is a feedback loop, not just a spurious refetch"
        );
    }

    /// The server's own log line, written through the real writer,
    /// reaches a subscriber as the log dataset — and as nothing else.
    /// The negative half is the one that matters: a `manage.rows` here
    /// would be the Manage screen refetching on its own log lines.
    #[tokio::test]
    async fn a_server_log_line_reaches_the_log_and_nothing_else() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);
        // Let the spawn read the store's counters before the first line
        // lands, so the line is what moves and not the file appearing.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let server = datalib_runs::ProcessLogWriter::start(
            td.path(),
            datalib_runs::Process::Http,
            None,
            datalib_runs::Retention::default(),
        )
        .unwrap();
        let mut n = 0;
        heard(
            &mut rx,
            RootEvent::TableChanged { table: Table::Log },
            || {
                n += 1;
                server.log(datalib_runs::LogRow {
                    level: "debug".into(),
                    msg: format!("line {n}"),
                    ..Default::default()
                });
            },
        )
        .await;
        drop(server);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        while let Ok(got) = rx.try_recv() {
            assert!(
                !matches!(
                    got.event,
                    RootEvent::TableChanged {
                        table: Table::ManageRows
                    }
                ),
                "a server log line was reported as a change to the Manage rows"
            );
        }
    }

    /// The grid index reports when its HEAD moves and not when its file
    /// does. The step writes the file throughout a pass — the working
    /// set lives in it — and commits once at the end; a grid told to
    /// refetch on the writes would fetch the same HEAD again each time,
    /// and be told nothing when the rows it can read actually changed.
    ///
    /// The store is opened and closed around every write, as the step
    /// does: it is its own process and lets go of the store each pass.
    #[tokio::test]
    async fn the_grid_index_reports_its_commits_and_not_its_writes() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        // The applet creates the directory at boot, before the first
        // pass; the watch is armed on it from the start here too.
        let db = datalib_core::layout::grid_index_db(&root);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(root.clone(), tx);

        async fn write(db: &Path, id: i64, commit: bool) {
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
        async fn index_changed(rx: &mut broadcast::Receiver<RootFrame>) -> bool {
            matches!(
                tokio::time::timeout(Duration::from_millis(500), rx.recv()).await,
                Ok(Ok(RootFrame {
                    event: RootEvent::IndexChanged,
                    ..
                }))
            )
        }

        // Committing until heard, because the watch may start delivering
        // a little after `spawn` returns — the same shape as `heard`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut n = 0;
        loop {
            n += 1;
            write(&db, n, true).await;
            if index_changed(&mut rx).await {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no IndexChanged within 20s"
            );
        }
        // Every commit the loop made gets reported; let the reports land
        // before listening for one that must not come.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        while rx.try_recv().is_ok() {}

        // The watch is live. Now writes with no commit behind them: the
        // file moves, HEAD does not.
        for i in 100..110 {
            write(&db, i, false).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        while let Ok(got) = rx.try_recv() {
            assert_ne!(
                got.event,
                RootEvent::IndexChanged,
                "a write the step has not committed was reported as an index change"
            );
        }

        write(&db, 200, true).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !index_changed(&mut rx).await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the commit was not reported within 20s"
            );
        }
    }

    /// The control for the filter, and the reason `classify` is not
    /// simply "anything under `system/`".
    #[tokio::test]
    async fn writes_to_the_usage_store_are_not_reported() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let usage = td.path().join("system/usage.doltlite_db");
        for n in 0..20 {
            std::fs::write(&usage, format!("row {n}")).unwrap();
        }
        // A real sleep, and the one place in this change that earns
        // one: proving a *negative* means waiting, because there is no
        // event for "nothing happened". 1.5 s is five debounce windows,
        // so a report would have been published long before this.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let got = rx.try_recv();
        assert!(
            got.is_err(),
            "a write to the usage store was reported as a data-root change: {got:?}"
        );
    }
}
