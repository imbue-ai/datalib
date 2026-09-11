//! What changed in the data root, pushed instead of polled.
//!
//! Rides the same SSE connection as job progress, as named `root` frames, so
//! a client has one connection, one reconnect policy, and one heartbeat to
//! judge liveness by.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::broadcast;

/// How long to hold a burst of filesystem events before publishing.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// How often to publish a [`RootEvent::Heartbeat`] on an otherwise
/// silent stream.
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// Something in the data root moved, or the stream is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootEvent {
    /// `<root>/config.toml` was written — by this server's own
    /// `PUT /api/config`, by an agent, or by hand in an editor.
    ConfigChanged,
    /// The runner's record or its run store moved:
    /// `system/dag_state.json`, `system/runs.sqlite`. This is the
    /// one that covers a `datalib-dag` run started from a terminal,
    /// which the job stream can never see because no job row exists
    /// for it.
    DagChanged,
    /// A component appeared, changed or vanished under
    /// `system/frontend/`.
    FrontendChanged,
    /// Nothing changed; the stream is open. See [`HEARTBEAT`].
    Heartbeat,
}

/// Fan-out channel for [`RootEvent`]s. Subscribed by
/// `GET /api/sync/stream` alongside the job channel.
pub type RootTx = broadcast::Sender<RootEvent>;

fn classify(root: &Path, path: &Path) -> Option<RootEvent> {
    // The atomic-write temp files are the same change reported twice;
    // the rename that follows is the one worth reporting.
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".tmp") {
        return None;
    }
    if path == root.join("config.toml") {
        return Some(RootEvent::ConfigChanged);
    }
    let system = root.join("system");
    if path.starts_with(system.join("frontend")) {
        return Some(RootEvent::FrontendChanged);
    }
    if path.parent() == Some(system.as_path()) {
        // `runs.sqlite-wal` / `-journal` are the same write as the
        // database itself, so match on the stem rather than equality.
        if name == "dag_state.json" || name.starts_with("runs.sqlite") {
            return Some(RootEvent::DagChanged);
        }
    }
    None
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

    // notify calls back on its own thread, so hand off through an
    // unbounded channel rather than doing any work there.
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<RootEvent>();
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
                if let Some(kind) = classify(&watch_root, path) {
                    let _ = raw_tx.send(kind);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
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
            eprintln!("watch: {} ({e})", dir.display());
        }
    }

    tokio::spawn(async move {
        // The debounce task owns the watcher, because dropping a
        // watcher stops the watch and there is nowhere better to put
        // it: `AppState` is cloned per request, and a handle nobody
        // ever calls would be state for its own sake. The task cannot
        // end — the only sender lives in the watcher's callback, which
        // this task now holds — so the watch lasts as long as the
        // process, which is exactly its intended lifetime.
        let _watcher = watcher;
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
            while let Ok(Some(kind)) = tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                pending.insert(kind);
            }
            for kind in pending {
                let _ = tx.send(kind);
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
            Some(RootEvent::ConfigChanged)
        );
        assert_eq!(
            classify(root, &root.join("system/dag_state.json")),
            Some(RootEvent::DagChanged)
        );
        assert_eq!(
            classify(root, &root.join("system/runs.sqlite-wal")),
            Some(RootEvent::DagChanged)
        );
        assert_eq!(
            classify(root, &root.join("system/frontend/user/abc.js")),
            Some(RootEvent::FrontendChanged)
        );
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
            "system/job-logs/abc.log",
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
        heard(&mut rx, RootEvent::DagChanged, move || {
            n += 1;
            let tmp = system.join("dag_state.json.tmp");
            std::fs::write(&tmp, format!("{{\"n\":{n}}}")).unwrap();
            std::fs::rename(&tmp, system.join("dag_state.json")).unwrap();
        })
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
