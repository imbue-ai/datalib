//! `datalib-pipeline serve` holds the loop until it is told to stop: syncs
//! asked for from other shells are served by it one after another, it
//! says on stderr what no store holds, and SIGTERM lets the root go.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn dag_bin() -> String {
    let p = Path::new(&std::env::var("DATALIB_PIPELINE_BIN").expect("DATALIB_PIPELINE_BIN is set"))
        .canonicalize()
        .expect("DATALIB_PIPELINE_BIN");
    p.to_string_lossy().into_owned()
}

/// One source that adds a line to `a/src/runs` each time it runs.
fn root_with_a_counting_source() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(
        td.path().join("config.toml"),
        "[[steps]]\nid = \"a/src\"\ncommand = \"sh -c 'mkdir -p a/src; echo ran >> a/src/runs'\"\n",
    )
    .unwrap();
    td
}

fn sync(root: &Path) -> Output {
    Command::new(dag_bin())
        .arg("sync")
        .arg(root)
        .args(["a/src", "--by", "test"])
        .output()
        .expect("run datalib-pipeline")
}

fn runs(root: &Path) -> usize {
    std::fs::read_to_string(root.join("a/src/runs"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Every event `serve` says, each line's `event` field, as it arrives.
struct Events {
    rx: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Events {
    fn of(stderr: std::process::ChildStderr) -> Events {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let value: serde_json::Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("not an NDJSON event ({e}): {line}"));
                let name = value["event"].as_str().expect("an event name").to_string();
                if tx.send(name).is_err() {
                    break;
                }
            }
        });
        Events {
            rx,
            seen: Vec::new(),
        }
    }

    fn wait_for(&mut self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(event) => {
                    self.seen.push(event.clone());
                    if event == name {
                        return;
                    }
                }
                Err(_) => panic!("serve never said {name}; it said {:?}", self.seen),
            }
        }
    }
}

#[test]
fn serve_runs_one_sync_after_another_until_sigterm() {
    let td = root_with_a_counting_source();
    let root = td.path();
    let mut serve = Command::new(dag_bin())
        .arg("serve")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn datalib-pipeline serve");
    let mut events = Events::of(serve.stderr.take().expect("piped stderr"));
    events.wait_for("holding");
    events.wait_for("prepare_done");

    for n in 1..=2 {
        let out = sync(root);
        assert_eq!(out.status.code(), Some(0), "sync {n}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("following request"),
            "sync {n} was not served by `serve`, which should still hold the loop: {out:?}"
        );
        events.wait_for("sync_over");
        assert_eq!(runs(root), n, "the source runs once per sync");
    }
    assert!(
        events.seen.iter().any(|e| e == "run_store_written"),
        "a sync writes the run store, and serve says so: {:?}",
        events.seen
    );

    // SAFETY: a plain kill(2) of a child this test spawned.
    unsafe {
        libc::kill(serve.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = serve.try_wait().expect("wait for serve") {
            break status;
        }
        assert!(Instant::now() < deadline, "serve did not exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0), "{status:?}");

    let out = sync(root);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("following request"),
        "serve let go of the lock, so a sync runs the loop itself: {out:?}"
    );
    assert_eq!(runs(root), 3);
}
