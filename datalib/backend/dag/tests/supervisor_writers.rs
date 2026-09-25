//! Seven processes on one `supervisor.sqlite` at once — three writing the
//! mailbox, two saving the loop's record, two reading as the server does —
//! in the rollback-journal mode the store is really in. Passes only if
//! every write lands and no process gives up on a lock.

use std::process::{Child, Command};

use datalib_dag::supervisor::store::Store;

const N: usize = 60;

fn start(role: &str, root: &std::path::Path, tag: &str) -> (String, Child) {
    let bin = std::env::var("WRITER_BIN").expect("WRITER_BIN, from the BUILD rule");
    let child = Command::new(bin)
        .args([role, &root.display().to_string(), tag, &N.to_string()])
        .spawn()
        .expect("start a writer");
    (format!("{role} {tag}"), child)
}

#[tokio::test]
async fn several_processes_share_the_store_and_every_write_lands() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let children: Vec<(String, Child)> = [
        ("mailbox", "m1"),
        ("mailbox", "m2"),
        ("mailbox", "m3"),
        ("record", "r1"),
        ("record", "r2"),
        ("reader", "q1"),
        ("reader", "q2"),
    ]
    .into_iter()
    .map(|(role, tag)| start(role, root, tag))
    .collect();
    for (who, mut child) in children {
        let status = child.wait().unwrap();
        assert!(status.success(), "{who} failed: {status}");
    }

    let store = Store::open(root).await.unwrap();
    let requests = store.recent_requests(10_000).await.unwrap();
    for tag in ["m1", "m2", "m3"] {
        let mine: Vec<_> = requests.iter().filter(|r| r.opened_by == tag).collect();
        assert_eq!(mine.len(), N, "{tag}'s requests");
        assert!(
            mine.iter()
                .all(|r| r.stop_requested_by.as_deref() == Some(tag)),
            "{tag}'s stops"
        );
    }
    assert!(
        store.paused().await.unwrap().is_empty(),
        "a resume was lost"
    );
    let record = store.load_record().await.unwrap();
    for tag in ["r1", "r2"] {
        for k in 0..5 {
            let last = (0..N).filter(|i| i % 5 == k).max().unwrap();
            assert_eq!(
                record.steps[&format!("{tag}/s{k}")].version.as_deref(),
                Some(last.to_string().as_str()),
                "{tag}/s{k}'s last save"
            );
        }
    }
    // The mode it runs in is the mode the store asks for.
    let header = std::fs::read(datalib_runtime::layout::supervisor_db(root)).unwrap();
    assert_eq!(&header[18..20], &[1, 1], "not in rollback-journal mode");
    store.close().await;
}
