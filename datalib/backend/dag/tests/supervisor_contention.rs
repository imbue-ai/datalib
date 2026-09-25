//! Seven processes on one `system/supervisor.sqlite` at once — people
//! opening requests, the loop saving its record, the server reading both
//! — and every write lands, every write is announced, and the file is in
//! the rollback-journal mode the docs say it is.

use std::path::Path;
use std::time::Duration;

use datalib_dag::supervisor::announce::{missed_announcements, Listener};
use datalib_dag::supervisor::store::Store;

const N: usize = 150;

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
    let mut listener = Listener::new(&store, "the test")
        .await
        .backstop(Duration::from_millis(20));

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
        // The listener drains the announcements as they come, as a live
        // one would, so its backstop judges each write.
        let out = loop {
            tokio::select! {
                out = &mut done => break out.unwrap(),
                _ = listener.next(&store) => {}
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
    assert!(store.paused().await.unwrap().is_empty());
    let db = datalib_runtime::layout::supervisor_db(root);
    assert_eq!(journal_mode_bytes(&db), [1, 1], "not rollback-journal");
    assert!(!db.with_extension("sqlite-wal").exists());
    assert_eq!(missed_announcements(), 0);
}
