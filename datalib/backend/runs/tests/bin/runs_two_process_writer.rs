//! One writer of the run store, as its own process: the runner's, or a
//! server's. Driven by `tests/runs_two_process_test.rs`, which is where
//! the scenario and the assertions live.
//!
//! It is a separate binary because the question under test is what
//! several *processes* do to one `runs.sqlite`. Writers inside one
//! process would share a SQLite library, a page cache and a set of
//! POSIX locks, and so would never take the path the store is asked to
//! survive: two processes contending for the write lock on one file.
//!
//! Each side writes one JSON report to `--out` and exits; nothing goes
//! to stdout, so a crashed child is distinguishable from a slow one.
//!
//! It also installs a subscriber, which matters more than it sounds. The
//! store reports a lost batch, a file it replaced and an open it could
//! not lock through `tracing`; a binary with no subscriber throws all of
//! that away. This test exists to catch silent loss, so a run of it that
//! cannot say *why* a writer lost its lines has caught the failure and
//! dropped the evidence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use datalib_runs::store::now_split;
use datalib_runs::{LogRow, MetricRow, Process, ProcessLogWriter, Retention, RunWriter};

fn main() {
    let mut argv = std::env::args().skip(1);
    let role = argv.next().expect("usage: <run|server> [--flag value]…");
    let args = Args::parse(argv);
    let out = args.path("out");
    capture_warnings(args.path("warn-log"));

    let (lines, process_id) = match role.as_str() {
        "run" => as_run(&args),
        "server" => as_server(&args),
        other => panic!("unknown role {other:?}"),
    };
    let report = serde_json::json!({ "role": role, "lines": lines, "process_id": process_id });
    std::fs::write(
        &out,
        serde_json::to_vec_pretty(&report).expect("the report is JSON"),
    )
    .unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
}

/// Send the store's own warnings to a file the test reads back, so a
/// failure names its cause instead of costing another investigation.
/// A fresh handle per event: these are rare, and it keeps the writer
/// free of a shared handle to reason about.
fn capture_warnings(path: PathBuf) {
    let _ = std::fs::write(&path, b"");
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|e| panic!("open the warning log: {e}"))
        })
        .init();
}

/// The runner's writer: a run with metrics as well as lines. Returns
/// how many lines it published and the process it published them under.
fn as_run(args: &Args) -> (u64, String) {
    let writer = RunWriter::start(
        &args.path("root"),
        args.str("run-id"),
        &now_split().0,
        None,
        Retention::default(),
    )
    .expect("start the run writer");
    let process_id = writer.process_id().to_string();
    let written = pound(args, |n| {
        writer.log(line(args.str("tag"), n));
        // Published per line and coalesced per flush, so the metric
        // and sample writes contend too.
        writer.metric(MetricRow {
            step: STEP.into(),
            name: "lines_written".into(),
            value: n as i64,
            updated_at_utc: now_split().0,
            ..Default::default()
        });
    });
    drop(writer);
    (written, process_id)
}

/// A server's writer: lines under no run at all.
fn as_server(args: &Args) -> (u64, String) {
    let writer = ProcessLogWriter::start(
        &args.path("root"),
        Process::Http,
        None,
        Retention::default(),
    )
    .expect("start the process writer");
    let process_id = writer.process_id().to_string();
    let written = pound(args, |n| writer.log(line(args.str("tag"), n)));
    drop(writer);
    (written, process_id)
}

/// Wait for the starting gun, then publish `--rounds` × `--per-round`
/// lines, pausing between rounds so the writer thread flushes many
/// times over the run rather than once at the end.
fn pound(args: &Args, mut publish: impl FnMut(u64)) -> u64 {
    await_file(&args.path("start"));
    let rounds = args.num("rounds", 30);
    let per_round = args.num("per-round", 100);
    let interval = Duration::from_millis(args.num("interval-ms", 100));
    for round in 0..rounds {
        for n in 0..per_round {
            publish(round * per_round + n);
        }
        std::thread::sleep(interval);
    }
    rounds * per_round
}

const STEP: &str = "two/process";

fn line(tag: &str, n: u64) -> LogRow {
    let (ts_utc, tz_offset) = now_split();
    LogRow {
        step: Some(STEP.into()),
        ts_utc,
        tz_offset,
        level: "info".into(),
        msg: format!("{tag}-{n}"),
        ..Default::default()
    }
}

/// Every writer waits on the same file, so they contend from their
/// first flush rather than starting a spawn apart.
fn await_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {}", path.display());
}

struct Args(HashMap<String, String>);

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Args {
        let mut map = HashMap::new();
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            let key = arg
                .strip_prefix("--")
                .unwrap_or_else(|| panic!("expected --flag, got {arg:?}"))
                .to_string();
            let takes_value = argv.peek().is_some_and(|v| !v.starts_with("--"));
            let value = if takes_value {
                argv.next().unwrap_or_default()
            } else {
                String::new()
            };
            map.insert(key, value);
        }
        Args(map)
    }

    fn str(&self, key: &str) -> &str {
        self.0
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("missing --{key}"))
    }

    fn path(&self, key: &str) -> PathBuf {
        PathBuf::from(self.str(key))
    }

    fn num(&self, key: &str, default: u64) -> u64 {
        self.0
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}
