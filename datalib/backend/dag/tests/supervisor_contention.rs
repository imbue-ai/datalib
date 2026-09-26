//! Seven processes on one `system/supervisor.sqlite` at once — people
//! opening requests, the loop saving its record, the server reading both
//! — and every write lands, every write is announced, and the file is in
//! the rollback-journal mode the docs say it is.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use datalib_dag::supervisor::announce::{Listener, UNANNOUNCED};
use datalib_dag::supervisor::store::Store;

const N: usize = 150;

/// Every line the seven announce, by kind: one per write, and one per
/// store opened.
fn expected() -> BTreeMap<&'static str, usize> {
    let switches = 3 * N.div_ceil(10);
    BTreeMap::from([
        ("store opened", 7),
        ("request opened", 3 * N),
        ("turned off", switches),
        ("turned on", switches),
        ("record saved", 2 * N),
    ])
}

/// Counts the lines heard by kind. The backstop's wake is not an
/// announcement, and a line of no known kind is a failure.
fn tally(heard: &mut BTreeMap<&'static str, usize>, lines: Vec<String>) {
    for line in lines.into_iter().filter(|l| l != UNANNOUNCED) {
        let kind = expected()
            .into_keys()
            .find(|k| line.starts_with(k))
            .unwrap_or_else(|| panic!("an announcement of no known kind: {line:?}"));
        *heard.entry(kind).or_default() += 1;
    }
}

/// Bytes 18 and 19 of a SQLite file: 1 for rollback-journal, 2 for WAL.
fn journal_mode_bytes(db: &Path) -> [u8; 2] {
    let bytes = std::fs::read(db).unwrap();
    [bytes[18], bytes[19]]
}

#[tokio::test(flavor = "multi_thread")]
async fn seven_processes_share_the_store_and_every_write_lands() {
    let worker = std::env::var("STORE_WORKER").expect("STORE_WORKER");
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let store = Store::open(root).await.unwrap();
    let mut listener = Listener::new(&store, "the test");
    let mut heard = BTreeMap::new();

    let roles = [
        ("mailbox", "m1"),
        ("mailbox", "m2"),
        ("mailbox", "m3"),
        ("record", "r1"),
        ("record", "r2"),
        ("reader", "q1"),
        ("reader", "q2"),
    ];
    // Spawned before any is awaited, so all seven run at once.
    let children: Vec<_> = roles
        .iter()
        .map(|(role, tag)| {
            let child = tokio::process::Command::new(&worker)
                .args([root.to_str().unwrap(), role, tag, &N.to_string()])
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            tokio::spawn(child.wait_with_output())
        })
        .collect();
    let mut outputs = Vec::new();
    for child in children {
        let done = child;
        tokio::pin!(done);
        // Drained as they come, as a live listener would: a pipe left to
        // fill drops what is announced into it.
        let out = loop {
            tokio::select! {
                out = &mut done => break out.unwrap(),
                lines = listener.next() => tally(&mut heard, lines),
            }
        };
        outputs.push(out);
    }
    for ((role, tag), out) in roles.iter().zip(outputs) {
        let out = out.unwrap();
        assert!(
            out.status.success(),
            "{role} {tag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    assert_eq!(store.open_requests().await.unwrap().len(), 3 * N);
    assert_eq!(store.load_record().await.unwrap().steps.len(), 2 * N);
    assert!(store.turned_off().await.unwrap().is_empty());
    let db = datalib_runtime::layout::supervisor_db(root);
    assert_eq!(journal_mode_bytes(&db), [1, 1], "not rollback-journal");
    assert!(!db.with_extension("sqlite-wal").exists());

    // Every write is announced once. A line can trail its commit by as
    // long as the writer is kept off the CPU, so what has not arrived yet
    // is waited for rather than counted as missed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while heard != expected() {
        match tokio::time::timeout_at(deadline, listener.next()).await {
            Ok(lines) => tally(&mut heard, lines),
            Err(_) => break,
        }
    }
    assert_eq!(heard, expected());
}
