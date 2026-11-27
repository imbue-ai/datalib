//! The loop's record: what each step last read and published, what
//! happened the last time a request reached it, and one row per process
//! the loop started. The loop holds it in memory as a
//! [`LoopState`] and saves what [`changes`] finds into
//! `system/supervisor.sqlite`. Only the process holding `runner-lock`
//! writes it; anyone may read it, with any `sqlite3`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sqlx::Row;

use serde::{Deserialize, Serialize};

use super::store::{RequestOutcome, Store};
use super::tick::{StateKind, WaitKind};
use crate::run_state::RunState;
use crate::step::{FailureKind, StepId};
use datalib_sql::{sql, Sql};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoopState {
    pub steps: BTreeMap<StepId, StepRow>,
}

/// What a step did the last time a request reached it: "what happened,
/// and when", beside [`StepRow`]'s "is it up to date". Its last
/// invocation, or, when a request found it up to date or blocked and ran
/// nothing, that outcome over the invocation before.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastInvocation {
    /// The step's last invocation — the key into
    /// `system/runs/runs.sqlite`, where its log lines and metrics live.
    /// Empty for a step no invocation of this build has run.
    pub invocation_id: String,
    /// The requests (or the job) it served; for an outcome with no
    /// invocation, the requests that reached the step.
    pub requests: Vec<String>,
    pub started_at: String,
    /// `None` while it is running.
    pub finished_at: Option<String>,
    /// How it ended, as `RunState::as_str`; empty while it runs.
    pub status: String,
    /// How many attempts this took, retries included.
    pub attempts: u32,
    pub error: Option<String>,
    /// Why it failed, when it did.
    pub failure: Option<FailureKind>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepRow {
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
    /// Its `--prepare` run at the loop's start reported `needs_rerun`:
    /// what it wrote is in a shape this build cannot reach in place. The
    /// tick treats it as due, and the loop runs it after the pass, until
    /// it next succeeds.
    pub needs_rerun: bool,
    /// What happened the last time a request reached it, whatever the
    /// outcome: what "last synced" says.
    pub last_invocation: Option<LastInvocation>,
    /// When a request last left it current: succeeded, or checked and
    /// found up to date. Kept here rather than read from the run store,
    /// which ages invocations out, and the source failing longest is the
    /// one whose last success matters most.
    pub last_success_at: Option<String>,
    /// What the loop's last tick made of it; `None` for a step no loop
    /// has ticked, or a word this build does not know.
    pub state: Option<StateKind>,
    /// What it waits on or is blocked by, or who turned it off: a sentence.
    pub state_detail: Option<String>,
    /// What kind of thing a `waiting` step waits for; `None` for a step in
    /// any other state, or a word this build does not know.
    pub waiting_for: Option<WaitKind>,
    pub turned_off_by: Option<String>,
    /// The open requests it has work left in, oldest first: what a Stop
    /// on its row stops. Stored as a JSON array, NULL when empty.
    pub requests: Vec<String>,
}

/// One thing the store must write to hold `next` where it held `prev`.
#[derive(Debug, PartialEq)]
pub enum Change<'a> {
    Step(&'a str, &'a StepRow),
    /// A step whose entry was removed, by a purge: its `steps` and
    /// `sinks` rows are deleted.
    Remove(&'a str),
}

/// What changed between two [`LoopState`]s, so a save writes that and nothing
/// else: the loop saves after every event, and most change one step.
pub fn changes<'a>(prev: &'a LoopState, next: &'a LoopState) -> Vec<Change<'a>> {
    let mut out = Vec::new();
    for (id, step) in &next.steps {
        if prev.steps.get(id) != Some(step) {
            out.push(Change::Step(id, step));
        }
    }
    for id in prev.steps.keys() {
        if !next.steps.contains_key(id) {
            out.push(Change::Remove(id));
        }
    }
    out
}

/// What a step read at its last success (`reads`, input path to
/// version), under which definition, and what happened the last time a
/// request reached it.
pub(super) const STEPS: &str = "CREATE TABLE IF NOT EXISTS steps (
        step TEXT PRIMARY KEY,
        succeeded INTEGER NOT NULL,
        fingerprint TEXT NOT NULL,
        reads TEXT NOT NULL,
        last_invocation_id TEXT,
        last_started_at_utc TEXT,
        last_finished_at_utc TEXT,
        last_status TEXT,
        last_attempts INTEGER,
        last_error TEXT,
        last_failure_kind TEXT,
        last_success_at_utc TEXT,
        tz_offset TEXT,
        state TEXT,
        state_detail TEXT,
        waiting_for TEXT,
        turned_off_by TEXT,
        requests TEXT,
        needs_rerun INTEGER NOT NULL DEFAULT 0,
        last_requests TEXT
    )";

pub(super) const DDL: [&str; 4] = [
    STEPS,
    // The version each tree was last published at, by its path (a
    // step's tree is its id).
    "CREATE TABLE IF NOT EXISTS sinks (
        path TEXT PRIMARY KEY,
        version TEXT NOT NULL
    )",
    // One per process the loop started. `outcome` is NULL while it runs.
    // The run store's `invocations` has the same id, and what it did.
    "CREATE TABLE IF NOT EXISTS invocations (
        id TEXT PRIMARY KEY,
        step TEXT NOT NULL,
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
    // How each step a request reached ended for it, written as the loop
    // closes the request. `outcome` is a `RunState` word.
    "CREATE TABLE IF NOT EXISTS request_steps (
        request_id TEXT NOT NULL,
        step TEXT NOT NULL,
        outcome TEXT NOT NULL,
        failure_kind TEXT,
        error TEXT,
        attempts INTEGER NOT NULL,
        failed_before INTEGER NOT NULL,
        changed INTEGER NOT NULL,
        PRIMARY KEY (request_id, step)
    )",
];

/// `invocations` keeps every process the loop ever started; this finds
/// the few still running without the rest.
pub(super) const INDEXES: [&str; 1] =
    ["CREATE INDEX IF NOT EXISTS invocations_open ON invocations (outcome, started_at_utc, id)"];

pub(super) const CLOSE_ABANDONED_INVOCATIONS: Sql = sql!(
    "dag.supervisor.close_abandoned_invocations",
    "UPDATE invocations INDEXED BY invocations_open \
     SET finished_at_utc = ?, outcome = ?, error = ? WHERE outcome IS NULL"
);

pub(super) const RUNNING_INVOCATIONS: Sql = sql!(
    "dag.supervisor.list_running_invocations",
    "SELECT id, step, started_at_utc FROM invocations INDEXED BY invocations_open \
     WHERE outcome IS NULL ORDER BY started_at_utc, id"
);

/// A process the loop started, as its row names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRow {
    pub id: String,
    pub step: String,
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

/// How one step's part in one request ended: its `request_steps` row, and
/// a line of `datalib-pipeline sync`'s report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestStep {
    pub step: StepId,
    pub outcome: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Of its last invocation for the request; 0 when none ran for it.
    pub attempts: u32,
    /// Its invocations for the request that failed before the last one.
    pub failed_before: u32,
    /// An invocation for the request moved the version of what it writes.
    pub changed: bool,
}

impl RequestStep {
    /// A step the request reached and ran nothing of.
    pub fn unrun(step: &str, outcome: RunState) -> RequestStep {
        RequestStep {
            step: step.to_string(),
            outcome,
            failure: None,
            error: None,
            attempts: 0,
            failed_before: 0,
            changed: false,
        }
    }

    /// Its line in `datalib-pipeline sync`'s report: the step, the
    /// [`RunState`] word, then what else there is to say.
    pub fn line(&self) -> String {
        let mut said = self.outcome.as_str().to_string();
        match self.outcome {
            RunState::Succeeded if self.changed => said.push_str(", changed its output"),
            RunState::Succeeded => said.push_str(", output unchanged"),
            RunState::Blocked => said.push_str(": a step it reads has published nothing"),
            _ => {}
        }
        if let Some(kind) = self.failure.filter(|_| self.outcome == RunState::Failed) {
            said.push_str(&format!(" ({}", kind.as_str()));
            if self.attempts > 1 {
                said.push_str(&format!(", {} attempts", self.attempts));
            }
            said.push(')');
        }
        if let Some(error) = &self.error {
            said.push_str(&format!(": {error}"));
        }
        match self.failed_before {
            0 => {}
            1 => said.push_str(
                "; 1 invocation failed before the last, and its error is in the step's log",
            ),
            n => said.push_str(&format!(
                "; {n} invocations failed before the last, and their errors are in the step's log"
            )),
        }
        format!("{:<32} {said}", self.step)
    }
}

impl Store {
    /// Close a request with how each step it reached ended for it, in one
    /// transaction, so a reader that sees it closed finds them too. The
    /// loop's to call, and only the loop's.
    pub async fn close_request_with_steps(
        &self,
        id: &str,
        outcome: RequestOutcome,
        failed_step: Option<&str>,
        steps: &[RequestStep],
    ) -> Result<()> {
        let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool().begin().await?;
        for s in steps {
            sql!(
                "dag.supervisor.insert_request_step",
                "INSERT OR REPLACE INTO request_steps (request_id, step, outcome, failure_kind, \
                 error, attempts, failed_before, changed) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .query()
            .bind(id)
            .bind(&s.step)
            .bind(s.outcome.as_str())
            .bind(s.failure.map(FailureKind::as_str))
            .bind(&s.error)
            .bind(i64::from(s.attempts))
            .bind(i64::from(s.failed_before))
            .bind(s.changed)
            .execute(&mut *tx)
            .await?;
        }
        sql!(
            "dag.supervisor.close_request_with_steps",
            "UPDATE requests SET closed_at_utc = ?, outcome = ?, failed_step = ? \
             WHERE id = ? AND closed_at_utc IS NULL"
        )
        .query()
        .bind(now)
        .bind(outcome.as_str())
        .bind(failed_step)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.announce(&format!("request closed {id}"));
        Ok(())
    }

    /// How each step a closed request reached ended for it.
    pub async fn request_steps(&self, request: &str) -> Result<Vec<RequestStep>> {
        sql!(
            "dag.supervisor.list_request_steps",
            "SELECT step, outcome, failure_kind, error, attempts, failed_before, changed \
             FROM request_steps WHERE request_id = ? ORDER BY step"
        )
        .query()
        .bind(request)
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(|r| {
            let outcome: String = r.try_get("outcome")?;
            Ok(RequestStep {
                step: r.try_get("step")?,
                outcome: RunState::parse(&outcome)
                    .with_context(|| format!("a request's step ended {outcome:?}"))?,
                failure: r
                    .try_get::<Option<String>, _>("failure_kind")?
                    .as_deref()
                    .and_then(FailureKind::parse),
                error: r.try_get("error")?,
                attempts: r.try_get::<i64, _>("attempts")?.try_into().unwrap_or(0),
                failed_before: r
                    .try_get::<i64, _>("failed_before")?
                    .try_into()
                    .unwrap_or(0),
                changed: r.try_get::<i64, _>("changed")? != 0,
            })
        })
        .collect()
    }

    /// The loop's tables as the last save left them.
    pub async fn load_state(&self) -> Result<LoopState> {
        let mut versions: BTreeMap<String, String> = BTreeMap::new();
        for r in sql!(
            "dag.supervisor.list_sink_versions",
            "SELECT path, version FROM sinks"
        )
        .query()
        .fetch_all(self.pool())
        .await?
        {
            versions.insert(r.try_get("path")?, r.try_get("version")?);
        }
        let mut steps = BTreeMap::new();
        for r in sql!("dag.supervisor.list_step_records", "SELECT * FROM steps")
            .query()
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
            let last_invocation = match r.try_get::<Option<String>, _>("last_invocation_id")? {
                None => None,
                Some(invocation_id) => Some(LastInvocation {
                    invocation_id,
                    requests: match r.try_get::<Option<String>, _>("last_requests")? {
                        Some(json) => {
                            serde_json::from_str(&json).context("a step's last requests")?
                        }
                        None => Vec::new(),
                    },
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
                    failure: r
                        .try_get::<Option<String>, _>("last_failure_kind")?
                        .as_deref()
                        .and_then(FailureKind::parse),
                }),
            };
            let state = StepRow {
                reads: serde_json::from_str(&reads).context("a step's reads")?,
                version: versions.remove(&id),
                succeeded: r.try_get::<i64, _>("succeeded")? != 0,
                fingerprint: r.try_get("fingerprint")?,
                needs_rerun: r.try_get::<i64, _>("needs_rerun")? != 0,
                last_invocation,
                last_success_at: joined("last_success_at_utc")?,
                state: r
                    .try_get::<Option<String>, _>("state")?
                    .as_deref()
                    .and_then(StateKind::parse),
                state_detail: r.try_get("state_detail")?,
                waiting_for: r
                    .try_get::<Option<String>, _>("waiting_for")?
                    .as_deref()
                    .and_then(WaitKind::parse),
                turned_off_by: r.try_get("turned_off_by")?,
                requests: match r.try_get::<Option<String>, _>("requests")? {
                    Some(json) => serde_json::from_str(&json).context("a step's requests")?,
                    None => Vec::new(),
                },
            };
            steps.insert(id, state);
        }
        Ok(LoopState { steps })
    }

    /// Write what changed from `prev` to `next`, in one transaction. The
    /// loop's to call, and only the loop's.
    pub async fn save_state(&self, prev: &LoopState, next: &LoopState) -> Result<()> {
        let changes = changes(prev, next);
        if changes.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await?;
        for change in changes {
            match change {
                Change::Step(id, st) => {
                    let utc =
                        |s: &Option<String>| s.as_deref().map(|s| datalib_time::split_stamp(s).utc);
                    let last = st.last_invocation.as_ref();
                    let offset = last
                        .map(|l| datalib_time::split_stamp(&l.started_at).tz_offset)
                        .or_else(|| {
                            st.last_success_at
                                .as_deref()
                                .map(|s| datalib_time::split_stamp(s).tz_offset)
                        })
                        .flatten();
                    sql!(
                        "dag.supervisor.upsert_step_record",
                        "INSERT OR REPLACE INTO steps (step, succeeded, fingerprint, reads, \
                         last_invocation_id, last_started_at_utc, last_finished_at_utc, \
                         last_status, last_attempts, last_error, last_failure_kind, \
                         last_success_at_utc, tz_offset, state, state_detail, waiting_for, \
                         turned_off_by, requests, needs_rerun, last_requests) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                    )
                    .query()
                    .bind(id)
                    .bind(st.succeeded)
                    .bind(&st.fingerprint)
                    .bind(serde_json::to_string(&st.reads)?)
                    .bind(last.map(|l| &l.invocation_id))
                    .bind(last.map(|l| datalib_time::split_stamp(&l.started_at).utc))
                    .bind(last.and_then(|l| utc(&l.finished_at)))
                    .bind(last.map(|l| &l.status))
                    .bind(last.map(|l| i64::from(l.attempts)))
                    .bind(last.and_then(|l| l.error.as_ref()))
                    .bind(last.and_then(|l| l.failure).map(FailureKind::as_str))
                    .bind(utc(&st.last_success_at))
                    .bind(offset)
                    .bind(st.state.map(StateKind::as_str))
                    .bind(&st.state_detail)
                    .bind(st.waiting_for.map(WaitKind::as_str))
                    .bind(&st.turned_off_by)
                    .bind(if st.requests.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&st.requests)?)
                    })
                    .bind(st.needs_rerun)
                    .bind(match last.map(|l| &l.requests) {
                        Some(requests) if !requests.is_empty() => {
                            Some(serde_json::to_string(requests)?)
                        }
                        _ => None,
                    })
                    .execute(&mut *tx)
                    .await?;
                    match &st.version {
                        Some(version) => {
                            sql!(
                                "dag.supervisor.upsert_sink_version",
                                "INSERT OR REPLACE INTO sinks (path, version) VALUES (?, ?)"
                            )
                            .query()
                            .bind(id)
                            .bind(version)
                            .execute(&mut *tx)
                            .await?;
                        }
                        None => {
                            sql!(
                                "dag.supervisor.delete_sink_version",
                                "DELETE FROM sinks WHERE path = ?"
                            )
                            .query()
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                        }
                    }
                }
                Change::Remove(id) => {
                    for stmt in [
                        sql!(
                            "dag.save_state.delete_steps",
                            "DELETE FROM steps WHERE step = ?"
                        ),
                        sql!(
                            "dag.save_state.delete_sinks",
                            "DELETE FROM sinks WHERE path = ?"
                        ),
                    ] {
                        stmt.query().bind(id).execute(&mut *tx).await?;
                    }
                }
            }
        }
        tx.commit().await?;
        self.announce("record saved");
        Ok(())
    }

    /// Whether a loop has started serving the request: some step's
    /// `requests` column names it, or it has already closed.
    pub async fn serving_started(&self, request: &str) -> Result<bool> {
        Ok(sql!("dag.supervisor.is_request_taken_on", "SELECT EXISTS(SELECT 1 FROM steps, json_each(steps.requests) WHERE json_each.value = ?1) \
             OR EXISTS(SELECT 1 FROM requests WHERE id = ?1 AND closed_at_utc IS NOT NULL)").query_scalar()
        .bind(request)
        .fetch_one(self.pool())
        .await?)
    }

    pub async fn open_invocation(&self, row: &InvocationRow) -> Result<()> {
        let started = datalib_time::split_stamp(&row.started_at_utc);
        sql!(
            "dag.supervisor.insert_invocation",
            "INSERT INTO invocations (id, step, started_at_utc, tz_offset) VALUES (?, ?, ?, ?)"
        )
        .query()
        .bind(&row.id)
        .bind(&row.step)
        .bind(&started.utc)
        .bind(&started.tz_offset)
        .execute(self.pool())
        .await?;
        self.announce(&format!("invocation opened {}", row.id));
        Ok(())
    }

    pub async fn close_invocation(&self, id: &str, end: &InvocationEnd) -> Result<()> {
        let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        sql!("dag.supervisor.close_invocation", "UPDATE invocations SET finished_at_utc = ?, outcome = ?, failure_kind = ?, error = ?, \
             attempts = ?, exit_code = ?, signal = ? WHERE id = ? AND outcome IS NULL").query()
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
        let closed = CLOSE_ABANDONED_INVOCATIONS
            .query()
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
        let rows = sql!(
            "dag.supervisor.list_invocations",
            "SELECT id, step, started_at_utc, outcome, failure_kind, error, attempts, \
             exit_code, signal FROM invocations ORDER BY rowid"
        )
        .query()
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let row = InvocationRow {
                    id: r.try_get("id")?,
                    step: r.try_get("step")?,
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
        RUNNING_INVOCATIONS
            .query()
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(|r| {
                Ok(InvocationRow {
                    id: r.try_get("id")?,
                    step: r.try_get("step")?,
                    started_at_utc: r.try_get("started_at_utc")?,
                })
            })
            .collect()
    }
}

/// The loop's tables at `root`, as a test reads them back.
#[cfg(test)]
pub(crate) async fn saved_state(root: &std::path::Path) -> LoopState {
    let store = Store::open(root).await.unwrap();
    let record = store.load_state().await.unwrap();
    store.close().await;
    record
}

/// Write `next` over the loop's tables at `root`, as a test sets them up.
#[cfg(test)]
pub(crate) async fn record(root: &std::path::Path, next: &LoopState) {
    let store = Store::open(root).await.unwrap();
    let prev = store.load_state().await.unwrap();
    store.save_state(&prev, next).await.unwrap();
    store.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use datalib_sql::test_sql;
    fn step(fingerprint: &str) -> StepRow {
        StepRow {
            succeeded: true,
            fingerprint: fingerprint.into(),
            version: Some("v1".into()),
            ..Default::default()
        }
    }

    fn stepped() -> LoopState {
        LoopState {
            steps: BTreeMap::from([(
                "slack/raw".into(),
                StepRow {
                    reads: BTreeMap::from([("x/raw".into(), "v0".into())]),
                    version: Some("abc".into()),
                    succeeded: true,
                    fingerprint: "fp-1".into(),
                    needs_rerun: true,
                    last_invocation: Some(LastInvocation {
                        invocation_id: "inv-1".into(),
                        requests: vec!["req-0".into()],
                        started_at: "2026-08-31T10:00:00+01:00".into(),
                        finished_at: Some("2026-08-31T10:00:09+01:00".into()),
                        status: "failed".into(),
                        attempts: 1,
                        error: Some("read /x: Operation not permitted".into()),
                        failure: Some(FailureKind::Permission),
                    }),
                    last_success_at: Some("2026-08-31T10:00:09+01:00".into()),
                    state: Some(StateKind::Waiting),
                    state_detail: Some("waiting for x/raw".into()),
                    waiting_for: Some(WaitKind::Upstream),
                    turned_off_by: Some("claude".into()),
                    requests: vec!["req-1".into(), "req-2".into()],
                },
            )]),
        }
    }

    /// The loop saves after every event; a save that rewrote every table
    /// each time would write every step on every tick.
    #[test]
    fn a_save_writes_what_changed_and_nothing_else() {
        let prev = LoopState {
            steps: BTreeMap::from([
                ("a/raw".into(), step("fp-a")),
                ("b/raw".into(), step("fp-b")),
                ("c/raw".into(), step("fp-c")),
            ]),
        };
        assert!(changes(&prev, &prev).is_empty());

        let mut next = prev.clone();
        next.steps.insert("b/raw".into(), step("fp-b2"));
        next.steps.remove("c/raw");
        assert_eq!(
            changes(&prev, &next),
            [
                Change::Step("b/raw", &next.steps["b/raw"]),
                Change::Remove("c/raw"),
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
        store.save_state(&LoopState::default(), &st).await.unwrap();
        assert_eq!(store.load_state().await.unwrap(), st);
    }

    /// A purge removes a step: its `steps` row and its tree's `sinks` row
    /// are deleted, so a step of that id later does its work from the
    /// start.
    #[tokio::test]
    async fn a_removed_step_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let st = stepped();
        store.save_state(&LoopState::default(), &st).await.unwrap();
        let mut reset = st.clone();
        reset.steps.remove("slack/raw");
        store.save_state(&st, &reset).await.unwrap();
        assert!(store.load_state().await.unwrap().steps.is_empty());
        let sinks: i64 = test_sql!("SELECT COUNT(*) FROM sinks")
            .query_scalar()
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
