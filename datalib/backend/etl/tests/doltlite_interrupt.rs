//! A doltlite writer killed inside a write, at each point a batch passes
//! through on its way to readers: mid-transaction, applied but not
//! committed, committed but not published, published. What a batch must be
//! everywhere is all or nothing, and what did not happen must happen when
//! the writer runs again:
//!
//! - a pinned reader — while the writer is stuck at the point, and after it
//!   is dead — sees the batch whole or not at all, and only once published;
//! - the next writer's `open` throws away what was never committed, and
//!   publishes what was committed and not published (`doltlite_raw::open`);
//! - running the batch again leaves it applied exactly once.
//!
//! This is storage's atomicity, the steps' own concern; the supervisor's
//! management of step processes is `dag`'s `supervisor_harness_test`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use datalib_etl::doltlite_raw;

const DEADLINE: Duration = Duration::from_secs(30);

/// The committed baseline, then the batch the writer is killed inside.
const BASE: &[&str] = &["put:a:1", "put:b:2", "put:c:3"];
const BATCH: &[&str] = &["put:a:10", "del:b", "put:d:4", "put:e:5"];
/// What the next run writes instead, touching none of the batch's rows.
const OTHER: &[&str] = &["put:z:9"];

fn writer(db: &Path, stop: &str, ops: &[&str]) -> Child {
    let bin = std::env::var("WRITER_BIN").expect("WRITER_BIN, from the BUILD rule");
    Command::new(bin)
        .arg(db)
        .arg(stop)
        .args(ops)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the writer")
}

/// The first line the writer prints, under a deadline: a hang is a failure
/// that says so, not a stuck test.
fn first_line(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    rx.recv_timeout(DEADLINE)
        .expect("the writer said nothing")
        .trim()
        .to_string()
}

fn apply(model: &mut BTreeMap<String, i64>, ops: &[&str]) {
    for op in ops {
        match op.split(':').collect::<Vec<_>>().as_slice() {
            ["put", id, n] => {
                model.insert(id.to_string(), n.parse().unwrap());
            }
            ["del", id] => {
                model.remove(*id);
            }
            _ => unreachable!(),
        }
    }
}

/// What a reader pinned at `main`'s head sees.
async fn seen(db: &Path) -> BTreeMap<String, i64> {
    let Some(reader) = doltlite_raw::open_reader(db, None).await.unwrap() else {
        return BTreeMap::new();
    };
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT id, n FROM pinned_rows")
        .fetch_all(reader.pool())
        .await
        .unwrap();
    reader.close().await;
    rows.into_iter().collect()
}

/// Run a writer to the end: what a download's next run does.
fn run(db: &Path, ops: &[&str]) {
    let mut child = writer(db, "none", ops);
    assert_eq!(first_line(&mut child), "done");
    assert!(child.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_killed_inside_a_batch_leaves_it_whole_or_not_at_all() {
    let mut before = BTreeMap::new();
    apply(&mut before, BASE);
    let mut after = before.clone();
    apply(&mut after, BATCH);

    // Where the batch stands for a reader once the writer is dead, and
    // once the next writer has opened the store.
    for (point, published_at_death, published_after_open) in [
        ("mid-tx", false, false),
        ("applied", false, false),
        ("committed", false, true),
        ("published", true, true),
    ] {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("store.doltlite_db");
        run(&db, BASE);

        let mut child = writer(&db, point, BATCH);
        assert_eq!(first_line(&mut child), format!("at {point}"));
        let want = |published: bool| if published { &after } else { &before };
        // A reader beside the stuck writer.
        assert_eq!(
            &seen(&db).await,
            want(point == "published"),
            "{point}: read beside the writer"
        );

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            &seen(&db).await,
            want(published_at_death),
            "{point}: read after its death"
        );

        // The next writer's open, and nothing after it, settles what the
        // dead one left.
        let mut next = writer(&db, "opened", &[]);
        assert_eq!(first_line(&mut next), "at opened");
        next.kill().unwrap();
        next.wait().unwrap();
        assert_eq!(
            &seen(&db).await,
            want(published_after_open),
            "{point}: read after the next open"
        );

        // Another batch now carries nothing of the dead writer's unless
        // it was committed: what was only applied is gone, not waiting to
        // ride along with the next commit.
        run(&db, OTHER);
        let mut then = want(published_after_open).clone();
        apply(&mut then, OTHER);
        assert_eq!(seen(&db).await, then, "{point}: the next batch");

        // And the batch run again happens exactly once.
        run(&db, BATCH);
        apply(&mut then, BATCH);
        assert_eq!(seen(&db).await, then, "{point}: the batch run again");
    }
}
