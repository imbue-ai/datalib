//! Two real processes against one `.doltlite_db`: can a reader pin an earlier
//! commit while a writer holds the store open and keeps committing, in either
//! order of opening?
//!
//! This is the premise the streaming-steps design rests on
//! (`docs/dev/streaming_steps_plan.md`), and it cannot be checked from inside
//! one process. Doltlite's working set lives in the *file* and is shared
//! across processes, and its chunk-store lock is a BSD `flock` on that file,
//! so two pools in one process share state that two processes do not. The
//! sibling unit tests in `etl/src/pin.rs` cover what pinning means; these
//! cover that it survives a second process writing underneath it.
//!
//! Each side runs as `//datalib/backend/etl:doltlite_two_process`, which
//! reports what it saw as JSON. This process opens no store of its own — a
//! coordinator holding a doltlite connection while it spawns would leak the
//! chunk-store flock into its children (`hack/doltlite_fork_bug/README.md`)
//! and the test would be measuring that instead.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Rows the seeding writer commits before the reader pins. Every sample the
/// reader takes must return exactly this, however far the writer has moved on.
const SEED_ROWS: i64 = 3;

/// A writer's `open` doing the ordinary thing takes milliseconds. This is not
/// a performance bound — it is the difference between "opened" and "blocked
/// behind the reader until the pool's 300s acquire timeout".
const OPEN_MUST_NOT_BLOCK: u64 = 30_000;

/// A writer holds the store open read-write and commits throughout, while a
/// second process opens it read-only, pins the commit that was HEAD when it
/// started, and reads. The pinned view must not move.
#[test]
fn a_reader_pins_an_earlier_commit_while_the_writer_keeps_committing() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--seed",
        "--pin-out",
        &t.path("pin"),
        "--until",
        &t.path("release"),
        "--max-commits",
        "200",
        "--interval-ms",
        "100",
        "--out",
        &t.path("writer.json"),
    ]);
    // The writer publishes the pin only once its seed commit has landed, so
    // reaching here means there is real committed state to pin to.
    let pin = t.await_file("pin", &mut writer);

    let mut reader = t.spawn(&[
        "read",
        "--db",
        &t.db(),
        "--pin",
        &pin,
        "--samples",
        "12",
        "--interval-ms",
        "250",
        "--ready-out",
        &t.path("reader-ready"),
        "--out",
        &t.path("reader.json"),
    ]);
    t.wait("reader", &mut reader);
    t.release(&mut writer);

    let writer = t.report("writer.json");
    if writer["dolt"] == Value::Bool(false) {
        return; // stock libsqlite3: no commits, nothing to pin.
    }
    let reader = t.report("reader.json");

    assert_stable_at_seed(&reader);
    assert_committed_throughout(&writer, &reader);
    assert_eq!(errors(&writer), Vec::<String>::new(), "writer errors");
    let probe = t.probe();
    assert!(
        probe["committed_rows"].as_i64().unwrap() > SEED_ROWS,
        "the writer's commits must have outrun the pin, or the reader's \
         stable view proves nothing: {probe:?}"
    );
}

/// The other order: the reader is already open and pinned when the writer
/// arrives. Opening read-write must not block behind it, and the commits that
/// follow must not disturb the reader's view.
#[test]
fn a_writer_opens_and_commits_under_a_reader_that_is_already_open() {
    let t = Scratch::new();
    // Seed in a process that then exits, so the reader is provably the first
    // one holding the store when the writer arrives.
    let mut seeder = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--seed",
        "--pin-out",
        &t.path("pin"),
        "--out",
        &t.path("seed.json"),
    ]);
    t.wait("seeder", &mut seeder);
    if t.report("seed.json")["dolt"] == Value::Bool(false) {
        return;
    }
    let pin = std::fs::read_to_string(t.dir.path().join("pin")).unwrap();

    let mut reader = t.spawn(&[
        "read",
        "--db",
        &t.db(),
        "--pin",
        &pin,
        "--samples",
        "16",
        "--interval-ms",
        "250",
        "--ready-out",
        &t.path("reader-ready"),
        "--out",
        &t.path("reader.json"),
    ]);
    // The reader signals only after its pool is open and its views installed,
    // so the writer below really is the second opener.
    t.await_file("reader-ready", &mut reader);

    let mut writer = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--until",
        &t.path("release"),
        "--max-commits",
        "200",
        "--interval-ms",
        "100",
        "--out",
        &t.path("writer.json"),
    ]);
    t.wait("reader", &mut reader);
    t.release(&mut writer);

    let writer = t.report("writer.json");
    let reader = t.report("reader.json");

    let open_ms = writer["open_ms"].as_u64().expect("open_ms");
    assert!(
        open_ms < OPEN_MUST_NOT_BLOCK,
        "opening read-write under a live reader took {open_ms}ms — it blocked"
    );
    assert_stable_at_seed(&reader);
    assert_committed_throughout(&writer, &reader);
    assert_eq!(errors(&writer), Vec::<String>::new(), "writer errors");
}

/// The shape `AGENTS.md`'s "One open per doltlite file" rule warns about, and
/// the one neither scenario above reaches: two read-write pools on one file
/// inside a single process. The rule is worth keeping — a second pool shares
/// the first's working set, so their `-Am` commits sweep up each other's rows
/// — but the reason given for it, that the second open waits on a file lock,
/// is what this measures. Bounded, so a platform where it really does wait
/// says so instead of hanging until the 300s acquire timeout.
#[test]
fn a_second_read_write_pool_does_not_block_on_the_first() {
    let t = Scratch::new();
    let mut child = t.spawn(&[
        "double-open",
        "--db",
        &t.db(),
        "--budget-ms",
        "20000",
        "--out",
        &t.path("double.json"),
    ]);
    t.wait("double-open", &mut child);
    let r = t.report("double.json");

    assert_eq!(r["second_open_error"], Value::Null, "second open: {r:?}");
    let open_ms = r["second_open_ms"].as_u64().expect("second_open_ms");
    assert!(
        open_ms < OPEN_MUST_NOT_BLOCK,
        "the second read-write open took {open_ms}ms — it waited on the first"
    );
    for commit in r["commits"].as_array().expect("commits") {
        assert_eq!(
            commit["error"],
            Value::Null,
            "committing through the {} pool: {r:?}",
            commit["pool"]
        );
    }
}

// ── assertions ──────────────────────────────────────────────────────

fn assert_stable_at_seed(reader: &Value) {
    assert_eq!(errors(reader), Vec::<String>::new(), "reader errors");
    let counts: Vec<i64> = samples(reader)
        .iter()
        .map(|s| s["count"].as_i64().expect("count"))
        .collect();
    assert!(
        counts.len() >= 8,
        "too few samples to mean anything: {counts:?}"
    );
    assert!(
        counts.iter().all(|&n| n == SEED_ROWS),
        "the pinned view moved under the reader: {counts:?}"
    );
}

/// The reader's window has to overlap commits the writer actually made, or a
/// stable view is just a view of a store nobody touched.
fn assert_committed_throughout(writer: &Value, reader: &Value) {
    let samples = samples(reader);
    let first = samples.first().expect("a sample")["at_ms"]
        .as_u64()
        .unwrap();
    let last = samples.last().expect("a sample")["at_ms"].as_u64().unwrap();
    let during: Vec<u64> = writer["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .filter_map(|c| c["at_ms"].as_u64())
        .filter(|at| (first..=last).contains(at))
        .collect();
    assert!(
        during.len() >= 2,
        "only {} commit(s) landed inside the reader's window ({first}..{last}); \
         the two never really overlapped",
        during.len()
    );
}

fn samples(report: &Value) -> &Vec<Value> {
    report["samples"].as_array().expect("samples")
}

fn errors(report: &Value) -> Vec<String> {
    report["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .map(|e| e.as_str().unwrap_or_default().to_string())
        .collect()
}

// ── driving the two processes ───────────────────────────────────────

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

    fn db(&self) -> String {
        self.path("store.doltlite_db")
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn spawn(&self, args: &[&str]) -> Child {
        Command::new(&self.bin)
            .args(args)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", self.bin.display()))
    }

    /// Poll for a rendezvous file, failing fast if the process that was
    /// supposed to write it has already exited.
    fn await_file(&self, name: &str, child: &mut Child) -> String {
        let path = self.dir.path().join(name);
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if path.exists() {
                return std::fs::read_to_string(&path).expect("read rendezvous file");
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!("the child exited ({status}) before writing {name}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for {name}");
    }

    fn wait(&self, who: &str, child: &mut Child) {
        let status = child.wait().expect("wait");
        assert!(status.success(), "the {who} exited {status}");
    }

    /// Tell a writer looping on `--until` to stop, then collect it.
    fn release(&self, writer: &mut Child) {
        std::fs::write(self.dir.path().join("release"), b"stop").expect("write release");
        self.wait("writer", writer);
    }

    fn report(&self, name: &str) -> Value {
        let path = self.dir.path().join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("the report is JSON")
    }

    /// What a fresh process sees now that both sides have let go.
    fn probe(&self) -> Value {
        let mut probe = self.spawn(&[
            "probe",
            "--db",
            &self.db(),
            "--out",
            &self.path("probe.json"),
        ]);
        self.wait("probe", &mut probe);
        self.report("probe.json")
    }
}
