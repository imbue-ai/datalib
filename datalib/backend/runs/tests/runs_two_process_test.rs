//! Four processes writing one `runs.sqlite` at once: does a line
//! published to the store always reach it?
//!
//! Two processes write this file on purpose — the runner writes its
//! runs, `datalib-http` writes its own log — and until this test the
//! reason that was safe had only ever been an argument. Four writers is
//! more than a data root ever really has, so a green run here is a
//! bound rather than a coincidence.
//!
//! What it catches: a writer losing the batch it was holding, whether
//! to a `SQLITE_BUSY` it could not wait out or to another process
//! deleting the file underneath it. A flush is one transaction over a
//! few hundred buffered lines, so one lost flush is hundreds of missing
//! lines, and nothing but a `warn!` would say so.
//!
//! Each writer is `//datalib/backend/runs:runs_two_process_writer`,
//! whose own file says why they are processes rather than threads.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use datalib_runs::{open_or_create, runs_path};
use serde_json::Value;

/// Lines each writer publishes, as rounds of that many. Spread over
/// `ROUNDS × INTERVAL_MS`, which at the store's 200ms flush is a dozen
/// or so flushes per writer with nothing keeping them in phase.
const ROUNDS: u64 = 30;
const PER_ROUND: u64 = 100;
const INTERVAL_MS: u64 = 100;

/// The runner, and three servers. Only one of each ever shares a real
/// data root; the extra two are load.
const WRITERS: &[(&str, &str)] = &[
    ("run", "runner"),
    ("server", "http-a"),
    ("server", "http-b"),
    ("server", "http-c"),
];

const RUN_ID: &str = "run-two-process";

/// Lines each writer is asked for.
const PUBLISHED: u64 = ROUNDS * PER_ROUND;

#[test]
fn every_line_four_writers_publish_reaches_the_store() {
    four_writers(&Scratch::new());
}

/// The same, onto a store this build has to remake: the file is already
/// there carrying another schema version, so every writer's open finds
/// a store it must delete and rebuild. Only one of them may actually do
/// it — a second delete would take the file the first is already
/// writing to, leaving it filling an inode nobody will ever read.
#[test]
fn a_store_from_another_schema_version_is_remade_once_under_four_writers() {
    let t = Scratch::new();
    t.plant_a_store_from_another_version();
    four_writers(&t);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM sqlite_master WHERE name = 'ancient'"),
        0,
        "the old store's table survived, so nothing remade the file"
    );
}

fn four_writers(t: &Scratch) {
    let mut children: Vec<(&str, Child)> = WRITERS
        .iter()
        .map(|(role, tag)| (*tag, t.spawn(role, tag)))
        .collect();

    // All four wait on this file, so their flushes overlap from the
    // first one rather than starting a spawn apart.
    t.go("start");
    let seen_while_writing = t.tail_until_they_finish(&mut children);
    for (tag, child) in &mut children {
        let status = child.wait().expect("wait");
        assert!(status.success(), "the {tag} writer exited {status}");
    }

    for (_, tag) in WRITERS {
        let report = t.report(tag);
        assert_eq!(report["lines"].as_u64(), Some(PUBLISHED), "{tag}: {report}");
        let stored = t.line_numbers(tag);
        let missing: Vec<u64> = (0..PUBLISHED)
            .filter(|n| stored.binary_search(n).is_err())
            .collect();
        assert!(
            missing.is_empty(),
            "the {tag} writer published {PUBLISHED} lines and {} never reached \
             the store, starting at {:?}",
            missing.len(),
            &missing[..missing.len().min(10)]
        );
        assert_eq!(
            stored.len() as u64,
            PUBLISHED,
            "the {tag} writer's lines are in the store more than once"
        );
    }
    assert_eq!(
        t.count("SELECT COUNT(*) FROM processes"),
        WRITERS.len() as i64,
        "every writer records the process it wrote under"
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM runs"),
        1,
        "the one run is recorded once"
    );
    assert!(
        seen_while_writing > 0,
        "the reader never saw a line while a writer was still going, so it \
         never read across a write"
    );
}

struct Scratch {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let bin = PathBuf::from(
            std::env::var("HELPER_BIN").expect("HELPER_BIN is set by the BUILD rule"),
        )
        .canonicalize()
        .expect("the helper binary is in runfiles");
        Scratch {
            dir: tempfile::tempdir().expect("tempdir"),
            bin,
        }
    }

    /// Read the store the way `datalib-http` does — open, query,
    /// close — for as long as the writers are going. A read must never
    /// fail while four writers contend, and what it counts must never
    /// go backwards. Returns the most it saw while one was still live.
    fn tail_until_they_finish(&self, children: &mut [(&str, Child)]) -> i64 {
        let mut last = 0;
        loop {
            let live = children
                .iter_mut()
                .any(|(_, c)| c.try_wait().expect("try_wait").is_none());
            let count = self.count("SELECT COUNT(*) FROM log");
            assert!(
                count >= last,
                "the log shrank under the reader: {last} → {count}"
            );
            if !live {
                return last;
            }
            last = count;
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn spawn(&self, role: &str, tag: &str) -> Child {
        Command::new(&self.bin)
            .args([
                role,
                "--root",
                &self.path("root"),
                "--run-id",
                RUN_ID,
                "--tag",
                tag,
                "--rounds",
                &ROUNDS.to_string(),
                "--per-round",
                &PER_ROUND.to_string(),
                "--interval-ms",
                &INTERVAL_MS.to_string(),
                "--start",
                &self.path("start"),
                "--out",
                &self.path(&format!("{tag}.json")),
            ])
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", self.bin.display()))
    }

    fn go(&self, name: &str) {
        std::fs::write(self.dir.path().join(name), b"go").expect("write the starting gun");
    }

    fn report(&self, tag: &str) -> Value {
        let path = self.dir.path().join(format!("{tag}.json"));
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("the report is JSON")
    }

    /// The `n` of every `<tag>-<n>` line in the store, ascending. A
    /// writer publishes `0..PUBLISHED`, so anything else is a gap.
    fn line_numbers(&self, tag: &str) -> Vec<u64> {
        let msgs = self.read(|pool| {
            let like = format!("{tag}-%");
            async move {
                sqlx::query_scalar::<_, String>("SELECT msg FROM log WHERE msg LIKE ?")
                    .bind(like)
                    .fetch_all(&pool)
                    .await
            }
        });
        let mut ns: Vec<u64> = msgs
            .iter()
            .map(|msg| {
                msg.rsplit_once('-')
                    .and_then(|(_, n)| n.parse().ok())
                    .unwrap_or_else(|| panic!("a line this test did not write: {msg:?}"))
            })
            .collect();
        ns.sort_unstable();
        ns
    }

    fn count(&self, sql: &'static str) -> i64 {
        self.read(|pool| async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&pool).await })
    }

    /// A store already on disk carrying another build's schema, so the
    /// writers' opens all find a file they have to remake.
    fn plant_a_store_from_another_version(&self) {
        let db = runs_path(&self.dir.path().join("root"));
        block_on(async move {
            let pool = open_or_create(&db).await.expect("plant the store");
            sqlx::raw_sql("CREATE TABLE ancient (x TEXT)")
                .execute(&pool)
                .await
                .expect("plant a table");
            sqlx::raw_sql("PRAGMA user_version = 1")
                .execute(&pool)
                .await
                .expect("plant a version");
            pool.close().await;
        });
        assert_ne!(
            datalib_runs::SCHEMA_VERSION,
            1,
            "the planted version has caught up with this build's"
        );
    }

    /// One open, one read, one close — the shape a reader of this store
    /// takes, and the one that has to hold while writers contend.
    ///
    /// A missing table is a wait, not a failure: until a writer has
    /// installed the schema there is nothing to read, and a store being
    /// remade is briefly a file with no tables in it. Anything else the
    /// read says is the answer, including a store it has broken.
    fn read<T, F, Fut>(&self, f: F) -> T
    where
        F: Fn(sqlx::SqlitePool) -> Fut,
        Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
    {
        let db = runs_path(&self.dir.path().join("root"));
        let deadline = Instant::now() + Duration::from_secs(60);
        block_on(async move {
            let mut waiting_for = "a writer to create the store".to_string();
            while Instant::now() < deadline {
                if db.exists() {
                    let pool = open_or_create(&db).await.expect("open the store");
                    let out = f(pool.clone()).await;
                    pool.close().await;
                    match out {
                        Ok(v) => return v,
                        Err(e) if is_missing_table(&e) => waiting_for = e.to_string(),
                        Err(e) => panic!("reading the store: {e}"),
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the store never answered; waiting for {waiting_for}");
        })
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

/// A table that is not there yet, as opposed to a store that is broken.
fn is_missing_table(e: &sqlx::Error) -> bool {
    e.to_string().contains("no such table")
}
