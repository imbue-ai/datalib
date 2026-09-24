//! Two real processes against one `.doltlite_db`: can a reader pin an earlier
//! commit while a writer holds the store open and keeps committing, in either
//! order of opening?
//!
//! This is the premise the streaming-steps design rests on
//! (`docs/dev/plans/completed/streaming_steps_plan.md`), and it cannot be checked from inside
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

/// A reader that keeps re-opening — the shape `grid_index` has under
/// streaming, one open/pin/diff/read/close per source per pass — for as long
/// as a writer commits as fast as it can. Every reader step is a read, so the
/// writer must never see `commit conflict`.
///
/// It did (#400): `install_views` used to ask `dolt_status` whether the store
/// was dirty, and that one statement, issued from a read-only connection,
/// failed about one in a hundred of the writer's overlapping commits here.
/// The writer is the bounded side, because every commit grows the file and a
/// reader's open reads all of it: bounding the reader instead let a fast
/// writer turn this into minutes of ever-slower opens.
#[test]
fn a_churning_reader_never_makes_the_writers_commit_fail() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--seed",
        "--pin-out",
        &t.path("pin"),
        "--max-commits",
        "2000",
        "--interval-ms",
        "0",
        "--out",
        &t.path("writer.json"),
    ]);
    t.await_file("pin", &mut writer);

    let mut reader = t.spawn(&[
        "churn",
        "--db",
        &t.db(),
        "--until",
        &t.path("writer.json"),
        "--rounds",
        "100000",
        "--out",
        &t.path("churn.json"),
    ]);
    t.wait("writer", &mut writer);
    t.wait("reader", &mut reader);

    let writer = t.report("writer.json");
    if writer["dolt"] == Value::Bool(false) {
        return;
    }
    let reader = t.report("churn.json");
    let commits = writer["commits"].as_array().map_or(0, Vec::len);
    assert_eq!(
        errors(&reader),
        Vec::<String>::new(),
        "reader errors (writer commits={commits})"
    );
    assert_eq!(
        errors(&writer),
        Vec::<String>::new(),
        "writer errors (writer commits={commits}, reader opened={} pinned={})",
        reader["opened"],
        reader["pinned"]
    );
    assert!(
        reader["pinned"].as_u64().unwrap_or(0) > 0,
        "the reader never pinned anything, so it never read: {reader:?}"
    );
    assert_committed_throughout(&writer, &reader);
}

/// The Manage screen's commit-history panel reads `dolt_log`,
/// `dolt_commit_ancestors`, `dolt_diff_summary`, `dolt_diff_stat` and a
/// `COUNT(*)` per table, none of which the churning reader above issues.
/// Same bar: the writer must never see `commit conflict`.
#[test]
fn a_history_reader_never_makes_the_writers_commit_fail() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--seed",
        "--pin-out",
        &t.path("pin"),
        "--max-commits",
        "500",
        "--interval-ms",
        "0",
        "--out",
        &t.path("writer.json"),
    ]);
    t.await_file("pin", &mut writer);

    let mut reader = t.spawn(&[
        "history",
        "--db",
        &t.db(),
        "--until",
        &t.path("writer.json"),
        "--rounds",
        "100000",
        "--out",
        &t.path("history.json"),
    ]);
    t.wait("writer", &mut writer);
    t.wait("reader", &mut reader);

    let writer = t.report("writer.json");
    if writer["dolt"] == Value::Bool(false) {
        return;
    }
    let reader = t.report("history.json");
    let commits = writer["commits"].as_array().map_or(0, Vec::len);
    assert_eq!(
        errors(&reader),
        Vec::<String>::new(),
        "reader errors (writer commits={commits})"
    );
    assert_eq!(
        errors(&writer),
        Vec::<String>::new(),
        "writer errors (writer commits={commits}, reader opened={} commits_seen={})",
        reader["opened"],
        reader["commits_seen"]
    );
    assert!(
        reader["commits_seen"].as_u64().unwrap_or(0) > 1,
        "the reader never walked a history: {reader:?}"
    );
    assert_committed_throughout(&writer, &reader);
}

/// One writer per file, by construction: a second read-write open on a
/// store another process is writing is refused, at once and naming the
/// holder, instead of sharing its working set and failing one of the two
/// commits later. The first writer goes on committing as if nothing
/// happened. The kernel releases the lock when the holder dies, so a
/// SIGKILLed writer (the `hang` scenarios below) leaves no stale claim.
#[test]
fn a_second_writer_in_another_process_is_refused_and_told_who_holds_the_store() {
    let t = Scratch::new();
    let mut holder = t.spawn(&[
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
        "50",
        "--out",
        &t.path("holder.json"),
    ]);
    t.await_file("pin", &mut holder);
    let holder_pid = holder.id();

    let mut second = t.spawn(&[
        "write",
        "--db",
        &t.db(),
        "--max-commits",
        "5",
        "--out",
        &t.path("second.json"),
    ]);
    t.wait("second writer", &mut second);
    let second = t.report("second.json");
    let refusal = second["open_error"]
        .as_str()
        .unwrap_or_else(|| panic!("the second writer was not refused: {second:?}"));
    assert!(refusal.contains("already has a writer"), "{refusal}");
    assert!(
        refusal.contains(&format!("pid {holder_pid}")),
        "the refusal names the holder ({holder_pid}): {refusal}"
    );

    // Long enough for the holder to commit past the refusal.
    std::thread::sleep(Duration::from_millis(300));
    t.release(&mut holder);
    let holder = t.report("holder.json");
    if holder["dolt"] == Value::Bool(false) {
        return;
    }
    assert_eq!(errors(&holder), Vec::<String>::new(), "holder errors");
    assert!(
        holder["commits"].as_array().map_or(0, Vec::len) >= 2,
        "the holder kept committing through the refused open: {holder:?}"
    );
    // And the store is free once the holder is gone.
    let probe = t.probe();
    assert!(probe["committed_rows"].as_i64().unwrap() > SEED_ROWS);
}

/// The same inside one process: the second pool is refused rather than
/// opened onto the first's working set. What a second pool did to the
/// first's commits — `commit conflict` on whichever committed second —
/// is the reason, and no longer reachable.
#[test]
fn a_second_read_write_pool_in_one_process_is_refused() {
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

    let refusal = r["second_open_error"]
        .as_str()
        .unwrap_or_else(|| panic!("second open was not refused: {r:?}"));
    assert!(refusal.contains("already has a writer"), "{refusal}");
    let open_ms = r["second_open_ms"].as_u64().expect("second_open_ms");
    assert!(
        open_ms < OPEN_MUST_NOT_BLOCK,
        "the refusal took {open_ms}ms — it waited instead of refusing"
    );
    let commits = r["commits"].as_array().expect("commits");
    assert_eq!(
        commits.len(),
        1,
        "only the first pool exists to commit: {r:?}"
    );
    assert_eq!(
        commits[0]["error"],
        Value::Null,
        "the first still commits: {r:?}"
    );
}

/// A SQL transaction that never reached `COMMIT` leaves nothing behind. A
/// writer is `kill -9`ed with rows inserted inside an open transaction; the
/// next writer to open the store finds the working set clean — the seed
/// rows and only the seed rows, and nothing for `open` to discard.
#[test]
fn a_transaction_a_killed_writer_never_committed_leaves_no_rows_behind() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "hang",
        "--db",
        &t.db(),
        "--rows",
        "5",
        "--ready-out",
        &t.path("ready"),
        "--out",
        &t.path("hang.json"),
    ]);
    if t.await_file("ready", &mut writer) == "no-dolt" {
        writer.kill().expect("kill");
        return;
    }
    writer.kill().expect("kill -9 the writer mid-transaction");
    writer.wait().expect("reap");

    let r = t.reopen();
    assert_eq!(
        r["working_set_rows"].as_i64(),
        Some(SEED_ROWS),
        "rows from a transaction that never committed are in the working set: {r:?}"
    );
    assert_eq!(r["committed_rows"].as_i64(), Some(SEED_ROWS), "{r:?}");
    assert!(
        !committed_a_rescue(&r),
        "there was nothing dirty, yet `open` committed something: {r:?}"
    );
}

/// The other half of the same boundary: a SQL `COMMIT` puts rows in the
/// working set, and the working set lives in the file, so they outlive the
/// process that wrote them. They were never at a seal — the writer died
/// before its `dolt_commit` — so the next `open` discards them and the
/// store starts at its last commit. A SQL commit is durability, not
/// consistency; the dolt commit is the boundary readers are promised.
#[test]
fn rows_a_killed_writer_committed_at_the_sql_level_are_discarded_by_the_next_open() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "hang",
        "--db",
        &t.db(),
        "--rows",
        "5",
        "--commit",
        "--ready-out",
        &t.path("ready"),
        "--out",
        &t.path("hang.json"),
    ]);
    if t.await_file("ready", &mut writer) == "no-dolt" {
        writer.kill().expect("kill");
        return;
    }
    writer
        .kill()
        .expect("kill -9 the writer after its SQL commit");
    writer.wait().expect("reap");

    let r = t.reopen();
    assert_eq!(
        r["working_set_rows"].as_i64(),
        Some(SEED_ROWS),
        "the next open discards what the dead writer left in the working set: {r:?}"
    );
    assert_eq!(
        r["committed_rows"].as_i64(),
        Some(SEED_ROWS),
        "and HEAD is where the last dolt_commit left it: {r:?}"
    );
    assert!(
        !committed_a_rescue(&r),
        "open must not seal what it found: {r:?}"
    );
}

/// The rows `grid_index` loads on a pass, as a reader in another process
/// sees them at each step of that pass. The writer deletes every row and
/// loads a different number back inside one SQL transaction, `COMMIT`s
/// it, then `dolt_commit`s; the reader holds one read-only connection --
/// the shape the search applet holds -- and samples the bare table,
/// `dolt_hashof('HEAD')` and the count at that HEAD throughout.
///
/// What it measures, and what the applet's per-request pin rests on:
/// a transaction another process has open is invisible here (the reader
/// never sees the delete without the reload), and HEAD moves on a
/// connection that was open before the commit -- so a long-lived reader
/// can pin per request without reopening.
///
/// The `sql_committed` phase is the one that used to be frightening.
/// A writer on `main` that `COMMIT`ed at the SQL level but had not yet
/// `dolt_commit`ed showed its whole uncommitted batch to any working-set
/// read in any process, which is most of why readers pin at all. Writers
/// now work on `WRITER_BRANCH` and fast-forward `main` only at the seal,
/// so that phase shows a reader nothing: the working set it can see is
/// `main`'s, and `main` does not move until the batch is sealed. This
/// test is where that claim is checked against two real processes.
#[test]
fn what_a_reader_sees_while_a_writer_deletes_and_reloads() {
    let t = Scratch::new();
    let mut writer = t.spawn(&[
        "hold",
        "--db",
        &t.db(),
        "--pin-out",
        &t.path("pin"),
        "--delete-when",
        &t.path("delete-when"),
        "--deleted-out",
        &t.path("deleted"),
        "--reload-when",
        &t.path("reload-when"),
        "--reloaded-out",
        &t.path("reloaded"),
        "--sql-commit-when",
        &t.path("sql-commit-when"),
        "--sql-committed-out",
        &t.path("sql-committed"),
        "--dolt-commit-when",
        &t.path("dolt-commit-when"),
        "--dolt-committed-out",
        &t.path("dolt-committed"),
        "--out",
        &t.path("hold.json"),
    ]);
    let seed = t.await_file("pin", &mut writer);

    let mut reader = t.spawn(&[
        "watch",
        "--db",
        &t.db(),
        "--phase-file",
        &t.path("phase"),
        "--until",
        &t.path("release"),
        "--interval-ms",
        "20",
        "--ready-out",
        &t.path("reader-ready"),
        "--out",
        &t.path("watch.json"),
    ]);
    t.await_file("reader-ready", &mut reader);

    t.phase("before");
    std::thread::sleep(SETTLE);
    t.step(&mut writer, "delete-when", "deleted", "deleted");
    t.step(&mut writer, "reload-when", "reloaded", "reloaded");
    t.step(
        &mut writer,
        "sql-commit-when",
        "sql-committed",
        "sql_committed",
    );
    let commit = t.step(
        &mut writer,
        "dolt-commit-when",
        "dolt-committed",
        "dolt_committed",
    );
    t.go("release");
    t.wait("reader", &mut reader);
    t.wait("writer", &mut writer);

    let writer = t.report("hold.json");
    if writer["dolt"] == Value::Bool(false) {
        return;
    }
    let reader = t.report("watch.json");
    let seen = seen_by_phase(&reader);
    for phase in [
        "before",
        "deleted",
        "reloaded",
        "sql_committed",
        "dolt_committed",
    ] {
        let in_phase: Vec<&Seen> = seen.iter().filter(|s| s.phase == phase).collect();
        assert!(
            !in_phase.is_empty(),
            "no sample landed wholly inside {phase}"
        );
        let (working, head, pinned) = match phase {
            // A transaction the writer has open is the writer's alone:
            // neither the delete nor the reload reaches another process.
            "before" | "deleted" | "reloaded" => (SEED_ROWS, &seed, SEED_ROWS),
            // `COMMIT`ed at the SQL level, not yet sealed: the batch is on
            // the writer's branch, so a reader on `main` sees neither it
            // nor a moved HEAD. Before writers took a branch this read
            // `RELOAD_ROWS` -- the uncommitted batch, visible to everyone.
            "sql_committed" => (SEED_ROWS, &seed, SEED_ROWS),
            "dolt_committed" => (RELOAD_ROWS, &commit, RELOAD_ROWS),
            _ => unreachable!(),
        };
        for s in in_phase {
            assert_eq!(
                (s.working, s.head.as_deref(), s.pinned),
                (Some(working), Some(head.as_str()), Some(pinned)),
                "in {phase}: {s:?}"
            );
        }
    }
    let slow: Vec<&Value> = samples(&reader)
        .iter()
        .filter(|s| s["ms"].as_u64().unwrap_or(0) > 1_000)
        .collect();
    assert!(
        slow.is_empty(),
        "a read-only sample waited on the writer's transaction: {slow:?}"
    );
}

/// Rows the `hold` writer loads back after the delete; the helper's
/// `RELOAD_ROWS`.
const RELOAD_ROWS: i64 = 5;

/// How long the reader is left sampling in each phase.
const SETTLE: Duration = Duration::from_millis(300);

/// One thing the reader saw: the phase it was in, and what the bare
/// table, HEAD and the count at HEAD answered.
#[derive(Debug, PartialEq)]
struct Seen {
    phase: String,
    working: Option<i64>,
    head: Option<String>,
    pinned: Option<i64>,
}

/// Every sample that sat wholly inside one phase, deduplicated in order.
/// A sample that straddled a phase change, and one taken while the
/// writer was between phases, say nothing about either.
fn seen_by_phase(reader: &Value) -> Vec<Seen> {
    let mut out: Vec<Seen> = Vec::new();
    for s in samples(reader) {
        if s["phase"] != s["phase_after"] || s["phase"] == TRANSITION {
            continue;
        }
        assert_eq!(s["working_error"], Value::Null, "{s}");
        assert_eq!(s["head_error"], Value::Null, "{s}");
        assert_eq!(s["pinned_error"], Value::Null, "{s}");
        let row = Seen {
            phase: s["phase"].as_str().unwrap_or_default().to_string(),
            working: s["working"].as_i64(),
            head: s["head"].as_str().map(str::to_string),
            pinned: s["pinned"].as_i64(),
        };
        if out.last() != Some(&row) {
            out.push(row);
        }
    }
    out
}

/// The phase label while the writer is taking a step.
const TRANSITION: &str = "transition";

fn committed_a_rescue(reopen: &Value) -> bool {
    reopen["commit_messages"]
        .as_array()
        .expect("commit_messages")
        .iter()
        .any(|m| m.as_str().unwrap_or_default().starts_with("rescue:"))
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

    /// Tell the reader which phase the writer is in.
    fn phase(&self, name: &str) {
        let tmp = self.dir.path().join("phase.part");
        std::fs::write(&tmp, name).expect("write phase");
        std::fs::rename(&tmp, self.dir.path().join("phase")).expect("rename phase");
    }

    fn go(&self, name: &str) {
        std::fs::write(self.dir.path().join(name), b"go").expect("write go-file");
    }

    /// Have the `hold` writer take one step, then leave the reader
    /// sampling in the state it left behind. Returns what the writer
    /// reported for the step.
    fn step(&self, writer: &mut Child, go: &str, done: &str, phase: &str) -> String {
        self.phase(TRANSITION);
        self.go(go);
        let reported = self.await_file(done, writer);
        self.phase(phase);
        std::thread::sleep(SETTLE);
        reported
    }

    fn report(&self, name: &str) -> Value {
        let path = self.dir.path().join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("the report is JSON")
    }

    /// What the next writer finds when it opens the store.
    fn reopen(&self) -> Value {
        let mut child = self.spawn(&[
            "reopen",
            "--db",
            &self.db(),
            "--out",
            &self.path("reopen.json"),
        ]);
        self.wait("reopen", &mut child);
        self.report("reopen.json")
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
