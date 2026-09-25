//! One process's share of the load in `supervisor_writers_test`: many
//! short transactions on `system/supervisor.sqlite`, as a person, the loop
//! or a reader makes them. A process rather than a thread because the
//! question is what several processes contending for one file's locks do;
//! connections in one process share a SQLite library and its locks.
//!
//! Any error panics, so the exit status is the report.

use datalib_dag::supervisor::record::{Record, StepRecord};
use datalib_dag::supervisor::store::Store;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let [role, root, tag, n] = argv.as_slice() else {
        panic!("usage: <mailbox|record|reader> <root> <tag> <n>");
    };
    let n: usize = n.parse().expect("n");
    let store = Store::open(std::path::Path::new(root)).await.expect("open");
    match role.as_str() {
        // What a person and the UI write: requests, stops, pauses.
        "mailbox" => {
            for i in 0..n {
                let id = store
                    .open_request(&[format!("{tag}/source")], tag)
                    .await
                    .expect("open a request");
                let step = format!("{tag}/p{i}");
                store.pause(&step, tag).await.expect("pause");
                store.resume(&step).await.expect("resume");
                store.request_stop(&id, tag).await.expect("stop");
            }
        }
        // What the loop writes: its record, a step at a time.
        "record" => {
            let mut prev = Record::default();
            for i in 0..n {
                let mut next = prev.clone();
                next.steps.insert(
                    format!("{tag}/s{}", i % 5),
                    StepRecord {
                        version: Some(i.to_string()),
                        ..Default::default()
                    },
                );
                store
                    .save_record(&prev, &next)
                    .await
                    .expect("save the record");
                prev = next;
            }
        }
        // What the server reads for every Manage refetch.
        "reader" => {
            for _ in 0..n {
                store.load_record().await.expect("load the record");
                store.open_requests().await.expect("open requests");
                store.recent_requests(100).await.expect("recent requests");
                store.paused().await.expect("pauses");
            }
        }
        other => panic!("no role {other}"),
    }
    store.close().await;
}
