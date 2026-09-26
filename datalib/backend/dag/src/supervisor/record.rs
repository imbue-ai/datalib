//! The loop's record: what each step last read and published, what
//! happened the last time a run reached it, the run in flight, and one row
//! per process the loop started. The loop holds it in memory as a
//! [`Record`] and saves what [`changes`] finds into
//! `system/supervisor.sqlite`. Only the process holding `runner-lock`
//! writes it; anyone may read it, with any `sqlite3`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sqlx::Row;

use super::store::Store;
use super::tick::StateKind;
use crate::step::StepId;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub steps: BTreeMap<StepId, StepRecord>,
    /// The run in flight, or the one that finished last.
    pub current_run: Option<CurrentRun>,
}

/// One busy period of the loop: the stretch from taking a request on to
/// having none left.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CurrentRun {
    pub run_id: String,
    pub started_at: String,
    /// `None` while the run is in flight.
    pub finished_at: Option<String>,
    /// Step id → what it is doing in this run, as `RunState::as_str`. A
    /// step the run has not reached has no entry.
    pub states: BTreeMap<StepId, String>,
}

/// What a step did the last time a run reached it: "what happened, and
/// when", beside [`StepRecord`]'s "is it up to date".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastRun {
    /// The run this happened in — the key into `system/runs/runs.sqlite`,
    /// where the step's log lines and metrics for it live.
    pub run_id: String,
    pub started_at: String,
    /// `None` while it is running.
    pub finished_at: Option<String>,
    /// How it ended, as `RunState::as_str`; empty while it runs.
    pub status: String,
    /// How many attempts this took, retries included.
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepRecord {
    /// Each input's path → the version this step read at its last
    /// success. A failure never updates it, so the step stays out of date
    /// until it succeeds.
    pub reads: BTreeMap<String, String>,
    /// The version of the tree this step writes, after the last
    /// invocation that moved it — failed ones too, since what a failed
    /// step committed is read downstream.
    pub version: Option<String>,
    pub succeeded: bool,
    /// The step's definition (argv, params, env, inputs) as of its last
    /// success. A step whose fingerprint no longer matches is out of date
    /// however unchanged its inputs: that is how a config edit takes
    /// effect.
    pub fingerprint: String,
    /// What happened the last time a run reached it, whatever the
    /// outcome: what "last synced" says.
    pub last_run: Option<LastRun>,
    /// When a run last left it current: succeeded, or checked and found
    /// up to date. Kept here rather than read from the run store, which
    /// ages runs out, and the source failing longest is the one whose last
    /// success matters most.
    pub last_success_at: Option<String>,
    /// What the loop's last tick made of it; `None` for a step no loop
    /// has ticked, or a word this build does not know.
    pub state: Option<StateKind>,
    /// What it waits on or is blocked by, or who turned it off: a sentence.
    pub state_detail: Option<String>,
    pub turned_off_by: Option<String>,
    /// The open request it is being run for, the oldest if several are.
    pub request: Option<String>,
}

/// One thing the store must write to hold `next` where it held `prev`.
#[derive(Debug, PartialEq)]
pub enum Change<'a> {
    /// The run, and every step's state in it.
    Run(&'a CurrentRun),
    Step(&'a str, &'a StepRecord),
    /// A step whose record was dropped, by a reset.
    Forget(&'a str),
}

/// What changed between two records, so a save writes that and nothing
/// else: the loop saves after every event, and most change one step.
pub fn changes<'a>(prev: &'a Record, next: &'a Record) -> Vec<Change<'a>> {
    let mut out = Vec::new();
    if let Some(run) = next.current_run.as_ref() {
        if prev.current_run.as_ref() != Some(run) {
            out.push(Change::Run(run));
        }
    }
    for (id, step) in &next.steps {
        if prev.steps.get(id) != Some(step) {
            out.push(Change::Step(id, step));
        }
    }
    for id in prev.steps.keys() {
        if !next.steps.contains_key(id) {
            out.push(Change::Forget(id));
        }
    }
    out
}

pub(super) const DDL: [&str; 5] = [
    // One per busy period of the loop; the newest is the run in flight,
    // or the one that finished last.
    "CREATE TABLE IF NOT EXISTS runs (
        run_id TEXT PRIMARY KEY,
        started_at_utc TEXT NOT NULL,
        finished_at_utc TEXT,
        tz_offset TEXT
    )",
    // What each step is doing in the newest run; a step the run has not
    // reached has no row.
    "CREATE TABLE IF NOT EXISTS run_steps (
        run_id TEXT NOT NULL,
        step TEXT NOT NULL,
        state TEXT NOT NULL,
        PRIMARY KEY (run_id, step)
    )",
    // What a step read at its last success (`reads`, input path to
    // version), under which definition, and what happened the last time a
    // run reached it.
    "CREATE TABLE IF NOT EXISTS steps (
        step TEXT PRIMARY KEY,
        succeeded INTEGER NOT NULL,
        fingerprint TEXT NOT NULL,
        reads TEXT NOT NULL,
        last_run_id TEXT,
        last_started_at_utc TEXT,
        last_finished_at_utc TEXT,
        last_status TEXT,
        last_attempts INTEGER,
        last_error TEXT,
        last_success_at_utc TEXT,
        tz_offset TEXT,
        state TEXT,
        state_detail TEXT,
        turned_off_by TEXT,
        request TEXT
    )",
    // The version each tree was last published at, by its path (a
    // step's tree is its id).
    "CREATE TABLE IF NOT EXISTS sinks (
        path TEXT PRIMARY KEY,
        version TEXT NOT NULL
    )",
    // One per process the loop started. `outcome` is NULL while it runs.
    "CREATE TABLE IF NOT EXISTS invocations (
        id TEXT PRIMARY KEY,
        step TEXT NOT NULL,
        run_id TEXT NOT NULL,
        started_at_utc TEXT NOT NULL,
        finished_at_utc TEXT,
        tz_offset TEXT,
        outcome TEXT,
        failure_kind TEXT,
        error TEXT,
        attempts INTEGER,
        exit_code INTEGER,
        signal INTEGER
    )",
];

/// Columns added to a table after it first shipped: (table, column,
/// declaration).
pub(super) const ADDED_COLUMNS: [(&str, &str, &str); 4] = [
    ("steps", "state", "TEXT"),
    ("steps", "state_detail", "TEXT"),
    ("steps", "turned_off_by", "TEXT"),
    ("steps", "request", "TEXT"),
];

/// A process the loop started, as its row names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRow {
    pub id: String,
    pub step: String,
    pub run_id: String,
    pub started_at_utc: String,
}

/// How an invocation ended.
#[derive(Debug, Clone, Default)]
pub struct InvocationEnd {
    /// A `RunState`, as its `as_str`.
    pub outcome: String,
    pub failure_kind: Option<String>,
    pub error: Option<String>,
    pub attempts: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

impl Store {
    /// The record as the last save left it.
    pub async fn load_record(&self) -> Result<Record> {
        let run = sqlx::query(
            "SELECT run_id, started_at_utc, finished_at_utc, tz_offset FROM runs \
             ORDER BY rowid DESC LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await?;
        let current_run = match run {
            None => None,
            Some(r) => {
                let run_id: String = r.try_get("run_id")?;
                let offset: Option<String> = r.try_get("tz_offset")?;
                let joined = |utc: String| datalib_time::join_stamp(&utc, offset.as_deref());
                let states = sqlx::query("SELECT step, state FROM run_steps WHERE run_id = ?")
                    .bind(&run_id)
                    .fetch_all(self.pool())
                    .await?
                    .iter()
                    .map(|r| Ok((r.try_get("step")?, r.try_get("state")?)))
                    .collect::<Result<BTreeMap<String, String>>>()?;
                Some(CurrentRun {
                    run_id,
                    started_at: joined(r.try_get("started_at_utc")?),
                    finished_at: r
                        .try_get::<Option<String>, _>("finished_at_utc")?
                        .map(joined),
                    states,
                })
            }
        };

        let mut versions: BTreeMap<String, String> = BTreeMap::new();
        for r in sqlx::query("SELECT path, version FROM sinks")
            .fetch_all(self.pool())
            .await?
        {
            versions.insert(r.try_get("path")?, r.try_get("version")?);
        }
        let mut steps = BTreeMap::new();
        for r in sqlx::query("SELECT * FROM steps")
            .fetch_all(self.pool())
            .await?
        {
            let id: String = r.try_get("step")?;
            let reads: String = r.try_get("reads")?;
            let offset: Option<String> = r.try_get("tz_offset")?;
            let joined = |column: &str| -> Result<Option<String>> {
                Ok(r.try_get::<Option<String>, _>(column)?
                    .map(|utc| datalib_time::join_stamp(&utc, offset.as_deref())))
            };
            let last_run = match r.try_get::<Option<String>, _>("last_run_id")? {
                None => None,
                Some(run_id) => Some(LastRun {
                    run_id,
                    started_at: joined("last_started_at_utc")?.unwrap_or_default(),
                    finished_at: joined("last_finished_at_utc")?,
                    status: r
                        .try_get::<Option<String>, _>("last_status")?
                        .unwrap_or_default(),
                    attempts: r
                        .try_get::<Option<i64>, _>("last_attempts")?
                        .unwrap_or(0)
                        .try_into()
                        .unwrap_or(0),
                    error: r.try_get("last_error")?,
                }),
            };
            let state = StepRecord {
                reads: serde_json::from_str(&reads).context("a step's reads")?,
                version: versions.remove(&id),
                succeeded: r.try_get::<i64, _>("succeeded")? != 0,
                fingerprint: r.try_get("fingerprint")?,
                last_run,
                last_success_at: joined("last_success_at_utc")?,
                state: r
                    .try_get::<Option<String>, _>("state")?
                    .as_deref()
                    .and_then(StateKind::parse),
                state_detail: r.try_get("state_detail")?,
                turned_off_by: r.try_get("turned_off_by")?,
                request: r.try_get("request")?,
            };
            steps.insert(id, state);
        }
        Ok(Record { steps, current_run })
    }

    /// Write what changed from `prev` to `next`, in one transaction. The
    /// loop's to call, and only the loop's.
    pub async fn save_record(&self, prev: &Record, next: &Record) -> Result<()> {
        let changes = changes(prev, next);
        if changes.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await?;
        for change in changes {
            match change {
                Change::Run(run) => {
                    let started = datalib_time::split_stamp(&run.started_at);
                    let finished = run
                        .finished_at
                        .as_deref()
                        .map(|f| datalib_time::split_stamp(f).utc);
                    sqlx::query(
                        "INSERT INTO runs (run_id, started_at_utc, finished_at_utc, tz_offset) \
                         VALUES (?, ?, ?, ?) ON CONFLICT(run_id) DO UPDATE SET \
                         finished_at_utc = excluded.finished_at_utc",
                    )
                    .bind(&run.run_id)
                    .bind(&started.utc)
                    .bind(finished)
                    .bind(&started.tz_offset)
                    .execute(&mut *tx)
                    .await?;
                    // Only the newest run's states are ever read.
                    sqlx::query("DELETE FROM run_steps")
                        .execute(&mut *tx)
                        .await?;
                    for (step, state) in &run.states {
                        sqlx::query("INSERT INTO run_steps (run_id, step, state) VALUES (?, ?, ?)")
                            .bind(&run.run_id)
                            .bind(step)
                            .bind(state)
                            .execute(&mut *tx)
                            .await?;
                    }
                }
                Change::Step(id, st) => {
                    let utc =
                        |s: &Option<String>| s.as_deref().map(|s| datalib_time::split_stamp(s).utc);
                    let last = st.last_run.as_ref();
                    let offset = last
                        .map(|l| datalib_time::split_stamp(&l.started_at).tz_offset)
                        .or_else(|| {
                            st.last_success_at
                                .as_deref()
                                .map(|s| datalib_time::split_stamp(s).tz_offset)
                        })
                        .flatten();
                    sqlx::query(
                        "INSERT OR REPLACE INTO steps (step, succeeded, fingerprint, reads, \
                         last_run_id, last_started_at_utc, last_finished_at_utc, last_status, \
                         last_attempts, last_error, last_success_at_utc, tz_offset, state, \
                         state_detail, turned_off_by, request) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(id)
                    .bind(st.succeeded)
                    .bind(&st.fingerprint)
                    .bind(serde_json::to_string(&st.reads)?)
                    .bind(last.map(|l| &l.run_id))
                    .bind(last.map(|l| datalib_time::split_stamp(&l.started_at).utc))
                    .bind(last.and_then(|l| utc(&l.finished_at)))
                    .bind(last.map(|l| &l.status))
                    .bind(last.map(|l| i64::from(l.attempts)))
                    .bind(last.and_then(|l| l.error.as_ref()))
                    .bind(utc(&st.last_success_at))
                    .bind(offset)
                    .bind(st.state.map(StateKind::as_str))
                    .bind(&st.state_detail)
                    .bind(&st.turned_off_by)
                    .bind(&st.request)
                    .execute(&mut *tx)
                    .await?;
                    match &st.version {
                        Some(version) => {
                            sqlx::query(
                                "INSERT OR REPLACE INTO sinks (path, version) VALUES (?, ?)",
                            )
                            .bind(id)
                            .bind(version)
                            .execute(&mut *tx)
                            .await?;
                        }
                        None => {
                            sqlx::query("DELETE FROM sinks WHERE path = ?")
                                .bind(id)
                                .execute(&mut *tx)
                                .await?;
                        }
                    }
                }
                Change::Forget(id) => {
                    for stmt in [
                        "DELETE FROM steps WHERE step = ?",
                        "DELETE FROM sinks WHERE path = ?",
                    ] {
                        sqlx::query(stmt).bind(id).execute(&mut *tx).await?;
                    }
                }
            }
        }
        tx.commit().await?;
        self.announce("record saved");
        Ok(())
    }

    /// Whether a loop has taken the request on: a step's record names
    /// it, or it has already closed.
    pub async fn taken_on(&self, request: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM steps WHERE request = ?1) \
             OR EXISTS(SELECT 1 FROM requests WHERE id = ?1 AND closed_at_utc IS NOT NULL)",
        )
        .bind(request)
        .fetch_one(self.pool())
        .await?)
    }

    pub async fn open_invocation(&self, row: &InvocationRow) -> Result<()> {
        let started = datalib_time::split_stamp(&row.started_at_utc);
        sqlx::query(
            "INSERT INTO invocations (id, step, run_id, started_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.step)
        .bind(&row.run_id)
        .bind(&started.utc)
        .bind(&started.tz_offset)
        .execute(self.pool())
        .await?;
        self.announce(&format!("invocation opened {}", row.id));
        Ok(())
    }

    pub async fn close_invocation(&self, id: &str, end: &InvocationEnd) -> Result<()> {
        let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        sqlx::query(
            "UPDATE invocations SET finished_at_utc = ?, outcome = ?, failure_kind = ?, error = ?, \
             attempts = ?, exit_code = ?, signal = ? WHERE id = ? AND outcome IS NULL",
        )
        .bind(now)
        .bind(&end.outcome)
        .bind(&end.failure_kind)
        .bind(&end.error)
        .bind(i64::from(end.attempts))
        .bind(end.exit_code)
        .bind(end.signal)
        .bind(id)
        .execute(self.pool())
        .await?;
        self.announce(&format!("invocation closed {id}"));
        Ok(())
    }

    /// Close every invocation a dead loop left open, as stopped: its
    /// process died with that loop or soon after, and nobody saw how.
    /// For the process taking the lock. How many it closed.
    pub async fn close_abandoned_invocations(&self, why: &str) -> Result<u64> {
        let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let closed = sqlx::query(
            "UPDATE invocations SET finished_at_utc = ?, outcome = ?, error = ? \
             WHERE outcome IS NULL",
        )
        .bind(now)
        .bind(crate::run_state::RunState::Stopped.as_str())
        .bind(why)
        .execute(self.pool())
        .await?
        .rows_affected();
        self.announce("abandoned invocations closed");
        Ok(closed)
    }

    /// Every invocation the loop recorded, oldest first, each with how it
    /// ended once it has.
    pub async fn invocations(&self) -> Result<Vec<(InvocationRow, Option<InvocationEnd>)>> {
        let rows = sqlx::query(
            "SELECT id, step, run_id, started_at_utc, outcome, failure_kind, error, attempts, \
             exit_code, signal FROM invocations ORDER BY rowid",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let row = InvocationRow {
                    id: r.try_get("id")?,
                    step: r.try_get("step")?,
                    run_id: r.try_get("run_id")?,
                    started_at_utc: r.try_get("started_at_utc")?,
                };
                let outcome: Option<String> = r.try_get("outcome")?;
                let end = match outcome {
                    None => None,
                    Some(outcome) => Some(InvocationEnd {
                        outcome,
                        failure_kind: r.try_get("failure_kind")?,
                        error: r.try_get("error")?,
                        attempts: r.try_get::<Option<i64>, _>("attempts")?.unwrap_or(0) as u32,
                        exit_code: r.try_get("exit_code")?,
                        signal: r.try_get("signal")?,
                    }),
                };
                Ok((row, end))
            })
            .collect()
    }

    /// The processes the loop started and has not seen end.
    pub async fn running_invocations(&self) -> Result<Vec<InvocationRow>> {
        sqlx::query(
            "SELECT id, step, run_id, started_at_utc FROM invocations \
             WHERE outcome IS NULL ORDER BY started_at_utc, id",
        )
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(|r| {
            Ok(InvocationRow {
                id: r.try_get("id")?,
                step: r.try_get("step")?,
                run_id: r.try_get("run_id")?,
                started_at_utc: r.try_get("started_at_utc")?,
            })
        })
        .collect()
    }
}

/// The record at `root`, as a test reads it back.
#[cfg(test)]
pub(crate) async fn recorded(root: &std::path::Path) -> Record {
    let store = Store::open(root).await.unwrap();
    let record = store.load_record().await.unwrap();
    store.close().await;
    record
}

/// Write `next` over the record at `root`, as a test sets one up.
#[cfg(test)]
pub(crate) async fn record(root: &std::path::Path, next: &Record) {
    let store = Store::open(root).await.unwrap();
    let prev = store.load_record().await.unwrap();
    store.save_record(&prev, next).await.unwrap();
    store.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(fingerprint: &str) -> StepRecord {
        StepRecord {
            succeeded: true,
            fingerprint: fingerprint.into(),
            version: Some("v1".into()),
            ..Default::default()
        }
    }

    fn stepped() -> Record {
        Record {
            steps: BTreeMap::from([(
                "slack/raw".into(),
                StepRecord {
                    reads: BTreeMap::from([("x/raw".into(), "v0".into())]),
                    version: Some("abc".into()),
                    succeeded: true,
                    fingerprint: "fp-1".into(),
                    last_run: Some(LastRun {
                        run_id: "run-1".into(),
                        started_at: "2026-08-31T10:00:00+01:00".into(),
                        finished_at: Some("2026-08-31T10:00:09+01:00".into()),
                        status: "succeeded".into(),
                        attempts: 1,
                        error: None,
                    }),
                    last_success_at: Some("2026-08-31T10:00:09+01:00".into()),
                    state: Some(StateKind::Waiting),
                    state_detail: Some("waiting for x/raw".into()),
                    turned_off_by: Some("claude".into()),
                    request: Some("req-1".into()),
                },
            )]),
            current_run: Some(CurrentRun {
                run_id: "run-1".into(),
                started_at: "2026-08-31T10:00:00+01:00".into(),
                finished_at: Some("2026-08-31T10:00:10+01:00".into()),
                states: BTreeMap::from([("slack/raw".into(), "succeeded".into())]),
            }),
        }
    }

    /// The loop saves after every event; a save that rewrote the whole
    /// record each time would write every step on every tick.
    #[test]
    fn a_save_writes_what_changed_and_nothing_else() {
        let prev = Record {
            steps: BTreeMap::from([
                ("a/raw".into(), step("fp-a")),
                ("b/raw".into(), step("fp-b")),
                ("c/raw".into(), step("fp-c")),
            ]),
            current_run: Some(CurrentRun {
                run_id: "r1".into(),
                ..Default::default()
            }),
        };
        assert!(changes(&prev, &prev).is_empty());

        let mut next = prev.clone();
        next.steps.insert("b/raw".into(), step("fp-b2"));
        next.steps.remove("c/raw");
        next.current_run.as_mut().unwrap().states =
            BTreeMap::from([("b/raw".into(), "running".into())]);
        assert_eq!(
            changes(&prev, &next),
            [
                Change::Run(next.current_run.as_ref().unwrap()),
                Change::Step("b/raw", &next.steps["b/raw"]),
                Change::Forget("c/raw"),
            ]
        );
    }

    /// Stamps go into the tables as UTC and an offset, and come back as
    /// the strings they went in as.
    #[tokio::test]
    async fn the_record_reads_back_as_it_was_saved() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let st = stepped();
        store.save_record(&Record::default(), &st).await.unwrap();
        assert_eq!(store.load_record().await.unwrap(), st);
    }

    /// A reset forgets a step: its row and its tree's version go, so the
    /// next run does its work from the start.
    #[tokio::test]
    async fn a_forgotten_step_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let st = stepped();
        store.save_record(&Record::default(), &st).await.unwrap();
        let mut reset = st.clone();
        reset.steps.remove("slack/raw");
        store.save_record(&st, &reset).await.unwrap();
        assert!(store.load_record().await.unwrap().steps.is_empty());
        let sinks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sinks")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(sinks, 0);
    }

    #[tokio::test]
    async fn an_invocation_is_running_until_it_is_closed() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let row = InvocationRow {
            id: "i1".into(),
            step: "a/raw".into(),
            run_id: "r1".into(),
            started_at_utc: "2026-09-24T10:00:00+02:00".into(),
        };
        store.open_invocation(&row).await.unwrap();
        let running = store.running_invocations().await.unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].step, "a/raw");

        let end = InvocationEnd {
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        store.close_invocation("i1", &end).await.unwrap();
        assert!(store.running_invocations().await.unwrap().is_empty());
    }
}
