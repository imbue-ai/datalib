//! What changed in the data root, pushed instead of polled.
//!
//! Rides the same SSE connection as job progress, as named `root` frames, so
//! a client has one connection, one reconnect policy, and one heartbeat to
//! judge liveness by. A frame names a *dataset* a reader fetches, not the
//! file that moved: one file can feed several readers, and one file —
//! the run store — is written by two processes for two audiences. The
//! filesystem says a file moved; the store says which of its parts did
//! (`datalib_runs::versions`); this module turns both into the datasets
//! to fetch again.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use datalib_runs::StorePart;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::broadcast;

/// How long to hold a burst of filesystem events before publishing.
const DEBOUNCE: Duration = Duration::from_millis(300);

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
    /// `GET /api/dag`: the runner's record, `system/dag_state.json`,
    /// written on every step state change. Covers a `datalib-dag` run
    /// started from a terminal, which the job stream never sees because
    /// no job row exists for it.
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
    /// The grid index (`unified_index/grid_index/db.doltlite_db`) was
    /// written. Under streaming that happens many times per sync -- a
    /// `grid_index` pass per checkpoint -- and it is how rows reach the
    /// grid while the download that produced them is still running.
    IndexChanged,
    /// Nothing changed; the stream is open. See [`HEARTBEAT`].
    Heartbeat,
}

/// Fan-out channel for [`RootEvent`]s. Subscribed by
/// `GET /api/sync/stream` alongside the job channel.
pub type RootTx = broadcast::Sender<RootEvent>;

/// A file the watcher reports on. What the filesystem can say; the
/// datasets it feeds are `expand`'s business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Moved {
    Config,
    DagState,
    RunStore,
    Frontend,
    GridIndex,
}

fn classify(root: &Path, path: &Path) -> Option<Moved> {
    // The atomic-write temp files are the same change reported twice;
    // the rename that follows is the one worth reporting.
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".tmp") {
        return None;
    }
    if path == root.join("config.toml") {
        return Some(Moved::Config);
    }
    let system = root.join("system");
    if path.starts_with(system.join("frontend")) {
        return Some(Moved::Frontend);
    }
    if path.parent() == Some(system.as_path()) {
        if name == "dag_state.json" {
            return Some(Moved::DagState);
        }
        // `runs.sqlite-wal` / `-journal` are the same write as the
        // database itself, so match on the stem rather than equality.
        if name.starts_with("runs.sqlite") {
            return Some(Moved::RunStore);
        }
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

/// The frames one debounced burst of file moves becomes.
async fn expand(
    root: &Path,
    moved: &HashSet<Moved>,
    seen: &mut BTreeMap<StorePart, i64>,
) -> HashSet<RootEvent> {
    let mut out = HashSet::new();
    let table = |t: Table| RootEvent::TableChanged { table: t };
    for m in moved {
        match m {
            Moved::Config => {
                out.insert(RootEvent::ConfigChanged);
                out.insert(table(Table::Dag));
                out.insert(table(Table::ManageRows));
            }
            Moved::DagState => {
                out.insert(table(Table::Dag));
                out.insert(table(Table::ManageRows));
            }
            Moved::RunStore => {
                for part in moved_parts(&datalib_runs::versions(root).await, seen) {
                    out.extend(tables_of(part).iter().map(|t| table(*t)));
                }
            }
            Moved::Frontend => {
                out.insert(RootEvent::FrontendChanged);
            }
            Moved::GridIndex => {
                out.insert(RootEvent::IndexChanged);
            }
        }
    }
    out
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
            let _ = heartbeat_tx.send(RootEvent::Heartbeat);
        }
    });

    // Create the directories before watching them: a watch on a path
    // that does not exist yet is simply not registered, and `system/`
    // is absent on a data root that has never synced.
    let system = root.join("system");
    let _ = std::fs::create_dir_all(&system);
    let frontend = system.join("frontend");
    let _ = std::fs::create_dir_all(&frontend);

    // Resolve symlinks once, and classify against the resolved form.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let system = std::fs::canonicalize(&system).unwrap_or(system);
    let frontend = std::fs::canonicalize(&frontend).unwrap_or(frontend);
    // Not created here: `unified_index/` belongs to the steps and the
    // applet, and a root that has never synced has none. Watched once it
    // exists — see the debounce loop below.
    let grid_index = datalib_core::layout::grid_index_dir(&root);

    // notify calls back on its own thread, so hand off through an
    // unbounded channel rather than doing any work there.
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<Moved>();
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
        // The store's write counters as of the last burst. Started from
        // the store so the first burst reports only what moved in it,
        // not everything the store already held.
        let mut seen = datalib_runs::versions(&root).await;
        loop {
            // Open a window on the first event, then coalesce
            // everything that lands inside it. One burst → one message
            // per kind.
            let Some(first) = raw_rx.recv().await else {
                return;
            };
            let mut pending = HashSet::from([first]);
            let deadline = tokio::time::Instant::now() + DEBOUNCE;
            // Ends on the window closing (`Err`) or the sender going
            // away (`Ok(None)`) — both mean "publish what you have".
            while let Ok(Some(moved)) = tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                pending.insert(moved);
            }
            // The grid index's directory appears partway through the
            // first sync, and a watch on a path that did not exist was
            // never registered. The runner's record moves throughout a
            // run, so arming on it converges within that run.
            if !index_watched && pending.contains(&Moved::DagState) {
                index_watched = watcher
                    .watch(&grid_index, RecursiveMode::NonRecursive)
                    .is_ok();
            }
            for event in expand(&root, &pending, &mut seen).await {
                let _ = tx.send(event);
            }
        }
    });
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
            classify(root, &root.join("system/dag_state.json")),
            Some(Moved::DagState)
        );
        assert_eq!(
            classify(root, &root.join("system/runs.sqlite-wal")),
            Some(Moved::RunStore)
        );
        assert_eq!(
            classify(root, &root.join("system/frontend/user/abc.js")),
            Some(Moved::Frontend)
        );
        assert_eq!(
            classify(root, &root.join("unified_index/grid_index/db.doltlite_db")),
            Some(Moved::GridIndex)
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

    /// The reason the directory watch filters by name at all. `system/`
    /// holds the job queue, which is written on every job state change
    /// — traffic the *job* stream already carries. Reporting it as
    /// `DagChanged` would make every sync refetch the runner's record
    /// several times per job, which is the poll this is replacing.
    #[test]
    fn the_stores_beside_the_runner_record_are_not_the_runner_record() {
        let root = Path::new("/data");
        for quiet in [
            "system/jobs.doltlite_db",
            "system/feedback.doltlite_db",
            "system/api-token",
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
        assert_eq!(
            classify(root, &root.join("system/dag_state.json.tmp")),
            None
        );
    }

    async fn heard(
        rx: &mut broadcast::Receiver<RootEvent>,
        want: RootEvent,
        mut stimulus: impl FnMut(),
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            stimulus();
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Ok(got)) if got == want => return,
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

    /// The same for the runner's own record — the case the sync-job
    /// stream structurally cannot cover, because a `datalib-dag` run
    /// started from a terminal has no job row behind it.
    #[tokio::test]
    async fn a_terminal_runners_state_write_reaches_a_subscriber() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let system = td.path().join("system");
        let mut n = 0;
        heard(
            &mut rx,
            RootEvent::TableChanged { table: Table::Dag },
            move || {
                n += 1;
                let tmp = system.join("dag_state.json.tmp");
                std::fs::write(&tmp, format!("{{\"n\":{n}}}")).unwrap();
                std::fs::rename(&tmp, system.join("dag_state.json")).unwrap();
            },
        )
        .await;
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
                    got,
                    RootEvent::TableChanged {
                        table: Table::ManageRows
                    }
                ),
                "a server log line was reported as a change to the Manage rows"
            );
        }
    }

    /// The control for the filter, and the reason `classify` is not
    /// simply "anything under `system/`".
    #[tokio::test]
    async fn writes_to_the_job_store_are_not_reported() {
        let td = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(64);
        spawn(td.path().to_path_buf(), tx);

        let jobs = td.path().join("system/jobs.doltlite_db");
        for n in 0..20 {
            std::fs::write(&jobs, format!("row {n}")).unwrap();
        }
        // A real sleep, and the one place in this change that earns
        // one: proving a *negative* means waiting, because there is no
        // event for "nothing happened". 1.5 s is five debounce windows,
        // so a report would have been published long before this.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            rx.try_recv().is_err(),
            "a write to the job store was reported as a data-root change"
        );
    }
}
