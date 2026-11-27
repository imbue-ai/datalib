//! Seven processes on one `system/supervisor.sqlite` at once — people
//! opening requests, the loop saving its `steps` rows, the server reading
//! both — and every write lands, every write is announced, and the file is
//! in the rollback-journal mode the docs say it is.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use datalib_dag::supervisor::announce::{Listener, UNANNOUNCED};
use datalib_dag::supervisor::store::Store;
use datalib_sql::test_sql;

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
    assert_eq!(store.load_state().await.unwrap().steps.len(), 2 * N);
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

/// How a worker names the data root: as the absolute path, or relative
/// to the directory above it, from there, as a server given a relative
/// root does while its runner resolves the same root to an absolute one.
#[derive(Clone, Copy)]
enum Naming {
    Absolute,
    Relative,
}

/// The seven roles above, each naming the root as `naming` says, all at
/// once; what each wrote is then in the store. No announcements are
/// counted, so nothing need open the store first.
async fn seven_at_once(root: &Path, naming: impl Fn(usize) -> Naming) {
    // Absolute, since some workers start from another directory.
    let worker = std::path::absolute(std::env::var("STORE_WORKER").expect("STORE_WORKER")).unwrap();
    let roles = [
        ("mailbox", "m1"),
        ("record", "r1"),
        ("reader", "q1"),
        ("mailbox", "m2"),
        ("record", "r2"),
        ("reader", "q2"),
        ("mailbox", "m3"),
    ];
    let above = root.parent().unwrap();
    let relative = root.file_name().unwrap();
    let children: Vec<_> = roles
        .iter()
        .enumerate()
        .map(|(i, (role, tag))| {
            let mut cmd = tokio::process::Command::new(&worker);
            match naming(i) {
                Naming::Absolute => cmd.arg(root),
                Naming::Relative => cmd.current_dir(above).arg(relative),
            };
            let child = cmd
                .arg(role)
                .arg(tag)
                .arg(N.to_string())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            tokio::spawn(child.wait_with_output())
        })
        .collect();
    for ((role, tag), child) in roles.iter().zip(children) {
        let out = child.await.unwrap().unwrap();
        assert!(
            out.status.success(),
            "{role} {tag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let store = Store::open(root).await.unwrap();
    assert_eq!(store.open_requests().await.unwrap().len(), 3 * N);
    assert_eq!(store.load_state().await.unwrap().steps.len(), 2 * N);
    store.close().await;
}

/// On a root nobody has opened, the seven make the file at once, as a
/// new library's server and its runner do.
#[tokio::test(flavor = "multi_thread")]
async fn seven_processes_make_the_store_at_once() {
    let td = tempfile::tempdir().unwrap();
    seven_at_once(td.path(), |_| Naming::Absolute).await;
}

/// Some name the root by a relative path and some by the absolute one,
/// as a server started with a relative root and its runner do.
#[tokio::test(flavor = "multi_thread")]
async fn processes_naming_the_root_two_ways_share_the_store() {
    let td = tempfile::tempdir().unwrap();
    Store::open(td.path()).await.unwrap().close().await;
    seven_at_once(td.path(), |i| {
        if i % 2 == 0 {
            Naming::Relative
        } else {
            Naming::Absolute
        }
    })
    .await;
}

/// A listener made while its process holds the store's write lock
/// leaves the lock held. It once read the file's change counter through
/// a descriptor it then closed, and closing any descriptor of a file
/// drops every lock its process holds on it, SQLite's included
/// (fcntl(2)): another process could then write while this one thought it
/// held the lock, and this one's commit failed with a disk I/O error. A
/// server handler makes a listener while another handler writes.
#[tokio::test(flavor = "multi_thread")]
async fn a_listener_leaves_its_processs_write_lock_held() {
    use sqlx::Connection;
    let worker = std::env::var("STORE_WORKER").expect("STORE_WORKER");
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let store = Store::open(root).await.unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(
        datalib_runtime::plain_sqlite::uri(&datalib_runtime::layout::supervisor_db(root)),
    );
    let mut writer = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
    test_sql!("BEGIN IMMEDIATE")
        .query()
        .execute(&mut writer)
        .await
        .unwrap();
    let try_write = || async {
        let out = tokio::process::Command::new(&worker)
            .arg(root)
            .arg("try-write")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    assert_eq!(try_write().await, "held", "the write lock was not taken");

    let _listener = Listener::new(&store, "the test");
    assert_eq!(
        try_write().await,
        "held",
        "making a listener let the lock go"
    );

    test_sql!("ROLLBACK")
        .query()
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();
    store.close().await;
}
