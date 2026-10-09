//! The watch against the OS's own filesystem events: that they arrive
//! at all, through a symlinked root, and that reading is not a change.
//! Everything the watch does with a move once it has one is tested
//! deterministically in `src/watch.rs`, on a fed watch. These are the
//! few that need FSEvents or inotify, and run alone (`watch_os_test`):
//! fseventsd has held every client's events back for over a minute on a
//! disk busy with other tests.

use std::path::Path;
use std::time::Duration;

use datalib_http::watch::{spawn_with, RootEvent, RootFrame, Timing};
use tokio::sync::broadcast;

/// A hang guard on the OS delivering an event, not a wait.
const HEARD_WITHIN: Duration = Duration::from_secs(60);

fn at_once() -> Timing {
    Timing {
        debounce: Duration::ZERO,
        manage_rows_every: Duration::ZERO,
        heartbeat: Duration::from_secs(3600),
    }
}

async fn watching(root: &Path) -> broadcast::Receiver<RootFrame> {
    let (tx, rx) = broadcast::channel(1024);
    let ready = spawn_with(root.to_path_buf(), tx, at_once());
    tokio::time::timeout(HEARD_WITHIN, ready.wait())
        .await
        .expect("the watch never reported ready");
    rx
}

async fn until(rx: &mut broadcast::Receiver<RootFrame>, want: RootEvent) -> Vec<RootEvent> {
    let wait = async {
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
    };
    tokio::time::timeout(HEARD_WITHIN, wait)
        .await
        .unwrap_or_else(|_| panic!("the OS never delivered a move reported as {want:?}"))
}

/// An editor's atomic write to `config.toml` reaches a subscriber and
/// the loop, through the OS alone.
#[tokio::test]
async fn an_external_config_write_reaches_a_subscriber_and_the_loop() {
    use datalib_dag::supervisor::announce::{Listener, CONFIG_CHANGED};
    let td = tempfile::tempdir().unwrap();
    let store = datalib_dag::supervisor::store::Store::open(td.path())
        .await
        .unwrap();
    let mut the_loop = Listener::new(&store, "test");
    let mut rx = watching(td.path()).await;

    let tmp = td.path().join("config.tmp");
    std::fs::write(&tmp, "# rewritten\n").unwrap();
    std::fs::rename(&tmp, td.path().join("config.toml")).unwrap();
    until(&mut rx, RootEvent::ConfigChanged).await;
    let heard = tokio::time::timeout(HEARD_WITHIN, the_loop.next())
        .await
        .expect("the loop was never told");
    assert!(heard.iter().any(|l| l == CONFIG_CHANGED), "{heard:?}");
}

/// The OS reports the resolved path of a root watched through a link.
#[cfg(unix)]
#[tokio::test]
async fn a_root_behind_a_symlink_still_reports() {
    let td = tempfile::tempdir().unwrap();
    let real = td.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = td.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut rx = watching(&link).await;
    let tmp = real.join("config.tmp");
    std::fs::write(&tmp, "# rewritten\n").unwrap();
    std::fs::rename(&tmp, real.join("config.toml")).unwrap();
    until(&mut rx, RootEvent::ConfigChanged).await;
}

/// Reading the component store and the config, as `FrontendStore::scan`
/// does, is not a change: on Linux a read reported as one is a feedback
/// loop. A frontend write after the reads is the barrier — the OS
/// delivers in order, so a read reported as a change would come first.
#[tokio::test]
async fn reading_the_component_store_is_not_a_change_to_it() {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(td.path().join("system/frontend")).unwrap();
    std::fs::write(td.path().join("system/frontend/c.js"), "x").unwrap();
    let mut rx = watching(td.path()).await;
    let frontend = td.path().join("system/frontend");
    for _ in 0..20 {
        for e in std::fs::read_dir(&frontend).unwrap().flatten() {
            let _ = std::fs::read(e.path());
        }
        let _ = std::fs::read(td.path().join("config.toml"));
    }
    std::fs::write(frontend.join("barrier.js"), "").unwrap();
    let got = until(&mut rx, RootEvent::FrontendChanged).await;
    assert_eq!(got, vec![RootEvent::FrontendChanged]);
}
