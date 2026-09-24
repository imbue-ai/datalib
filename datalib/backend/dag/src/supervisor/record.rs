//! The loop's record in `system/supervisor.sqlite`: what each step last
//! did and read, what each sink holds, the run in flight, and one row per
//! process the loop started. Only the process holding `runner-lock`
//! writes these; anyone may read them, with any `sqlite3`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::Row;

use super::store::Store;
use crate::state::{changes, Change, CurrentRun, DagState, LastRun, StepState};

pub(super) const DDL: [&str; 5] = [
    // One per busy period of the loop; the newest is the run in flight,
    // or the one that finished last.
    "CREATE TABLE IF NOT EXISTS runs (
        run_id TEXT PRIMARY KEY,
        started_at_utc TEXT NOT NULL,
        finished_at_utc TEXT,
        tz_offset TEXT,
        plan TEXT NOT NULL
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
        tz_offset TEXT
    )",
    // The version of what a step published, by the tree it writes.
    "CREATE TABLE IF NOT EXISTS sinks (
        path TEXT PRIMARY KEY,
        step TEXT NOT NULL,
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
    pub async fn load_record(&self) -> Result<DagState> {
        let run = sqlx::query(
            "SELECT run_id, started_at_utc, finished_at_utc, tz_offset, plan FROM runs \
             ORDER BY rowid DESC LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await?;
        let current_run = match run {
            None => None,
            Some(r) => {
                let run_id: String = r.try_get("run_id")?;
                let plan: String = r.try_get("plan")?;
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
                    plan: serde_json::from_str(&plan).context("a run's plan")?,
                    states,
                })
            }
        };

        let mut outputs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for r in sqlx::query("SELECT path, step, version FROM sinks")
            .fetch_all(self.pool())
            .await?
        {
            outputs
                .entry(r.try_get("step")?)
                .or_default()
                .insert(r.try_get("path")?, r.try_get("version")?);
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
            let state = StepState {
                input_versions: serde_json::from_str(&reads).context("a step's reads")?,
                output_versions: outputs.remove(&id).unwrap_or_default(),
                succeeded: r.try_get::<i64, _>("succeeded")? != 0,
                fingerprint: r.try_get("fingerprint")?,
                last_run,
                last_success_at: joined("last_success_at_utc")?,
            };
            steps.insert(id, state);
        }
        Ok(DagState { steps, current_run })
    }

    /// Write what changed from `prev` to `next`, in one transaction. The
    /// loop's to call, and only the loop's.
    pub async fn save_record(&self, prev: &DagState, next: &DagState) -> Result<()> {
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
                        "INSERT INTO runs (run_id, started_at_utc, finished_at_utc, tz_offset, plan) \
                         VALUES (?, ?, ?, ?, ?) ON CONFLICT(run_id) DO UPDATE SET \
                         finished_at_utc = excluded.finished_at_utc, plan = excluded.plan",
                    )
                    .bind(&run.run_id)
                    .bind(&started.utc)
                    .bind(finished)
                    .bind(&started.tz_offset)
                    .bind(serde_json::to_string(&run.plan)?)
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
                         last_attempts, last_error, last_success_at_utc, tz_offset) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(id)
                    .bind(st.succeeded)
                    .bind(&st.fingerprint)
                    .bind(serde_json::to_string(&st.input_versions)?)
                    .bind(last.map(|l| &l.run_id))
                    .bind(last.map(|l| datalib_time::split_stamp(&l.started_at).utc))
                    .bind(last.and_then(|l| utc(&l.finished_at)))
                    .bind(last.map(|l| &l.status))
                    .bind(last.map(|l| i64::from(l.attempts)))
                    .bind(last.and_then(|l| l.error.as_ref()))
                    .bind(utc(&st.last_success_at))
                    .bind(offset)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("DELETE FROM sinks WHERE step = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    for (path, version) in &st.output_versions {
                        sqlx::query(
                            "INSERT OR REPLACE INTO sinks (path, step, version) VALUES (?, ?, ?)",
                        )
                        .bind(path)
                        .bind(id)
                        .bind(version)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                Change::Forget(id) => {
                    for stmt in [
                        "DELETE FROM steps WHERE step = ?",
                        "DELETE FROM sinks WHERE step = ?",
                    ] {
                        sqlx::query(stmt).bind(id).execute(&mut *tx).await?;
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Bring in the record a root kept as `system/dag_state.json`, if it
    /// has one and the store holds none yet, and set the file aside. For
    /// the process taking the lock.
    pub async fn import_legacy_record(&self, data_root: &Path) -> Result<bool> {
        let Some(legacy) = DagState::read_legacy_json(data_root)? else {
            return Ok(false);
        };
        let held: i64 =
            sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM steps) + (SELECT COUNT(*) FROM runs)")
                .fetch_one(self.pool())
                .await?;
        if held == 0 {
            self.save_record(&DagState::default(), &legacy).await?;
        }
        let path = data_root.join(crate::state::LEGACY_JSON_REL_PATH);
        let aside = path.with_extension("json.imported");
        std::fs::rename(&path, &aside)
            .with_context(|| format!("set {} aside as {}", path.display(), aside.display()))?;
        Ok(held == 0)
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
        Ok(closed)
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
pub(crate) async fn recorded(root: &Path) -> DagState {
    let store = Store::open(root).await.unwrap();
    let record = store.load_record().await.unwrap();
    store.close().await;
    record
}

/// Write `next` over the record at `root`, as a test sets one up.
#[cfg(test)]
pub(crate) async fn record(root: &Path, next: &DagState) {
    let store = Store::open(root).await.unwrap();
    let prev = store.load_record().await.unwrap();
    store.save_record(&prev, next).await.unwrap();
    store.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stepped() -> DagState {
        DagState {
            steps: BTreeMap::from([(
                "slack/raw".into(),
                StepState {
                    input_versions: BTreeMap::from([("x/raw".into(), "v0".into())]),
                    output_versions: BTreeMap::from([("slack/raw".into(), "abc".into())]),
                    succeeded: true,
                    fingerprint: "fp-1".into(),
                    last_run: Some(LastRun {
                        run_id: "run-1".into(),
                        started_at: "2026-08-31T09:00:00+00:00".into(),
                        finished_at: Some("2026-08-31T09:00:09+00:00".into()),
                        status: "succeeded".into(),
                        attempts: 1,
                        error: None,
                    }),
                    last_success_at: Some("2026-08-31T09:00:09+00:00".into()),
                },
            )]),
            current_run: Some(CurrentRun {
                run_id: "run-1".into(),
                started_at: "2026-08-31T09:00:00+00:00".into(),
                finished_at: Some("2026-08-31T09:00:10+00:00".into()),
                plan: vec!["slack/raw".into(), "slack/rendered_md".into()],
                states: BTreeMap::from([("slack/raw".into(), "succeeded".into())]),
            }),
        }
    }

    #[tokio::test]
    async fn the_record_reads_back_as_it_was_saved() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let st = stepped();
        store.save_record(&DagState::default(), &st).await.unwrap();

        let back = store.load_record().await.unwrap();
        let step = &back.steps["slack/raw"];
        assert!(step.succeeded);
        assert_eq!(step.fingerprint, "fp-1");
        assert_eq!(step.input_versions, st.steps["slack/raw"].input_versions);
        assert_eq!(step.output_versions["slack/raw"], "abc");
        let last = step.last_run.as_ref().expect("last_run survives the trip");
        assert_eq!((last.status.as_str(), last.attempts), ("succeeded", 1));
        assert_eq!(
            last.finished_at.as_deref(),
            Some("2026-08-31T09:00:09+00:00")
        );
        let run = back.current_run.expect("the run survives the trip");
        assert_eq!(run.plan.len(), 2);
        assert_eq!(run.states["slack/raw"], "succeeded");
        assert!(run.finished_at.is_some());
    }

    /// A reset forgets a step: its row and its sink's version go, so the
    /// next run does its work from the start.
    #[tokio::test]
    async fn a_forgotten_step_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let st = stepped();
        store.save_record(&DagState::default(), &st).await.unwrap();
        let mut reset = st.clone();
        reset.steps.remove("slack/raw");
        store.save_record(&st, &reset).await.unwrap();
        assert!(store.load_record().await.unwrap().steps.is_empty());
    }

    #[tokio::test]
    async fn a_legacy_json_record_is_imported_once_and_set_aside() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("system")).unwrap();
        let json = root.path().join(crate::state::LEGACY_JSON_REL_PATH);
        std::fs::write(&json, serde_json::to_vec(&stepped()).unwrap()).unwrap();
        let store = Store::open(root.path()).await.unwrap();

        assert!(store.import_legacy_record(root.path()).await.unwrap());
        assert!(!json.exists());
        assert!(json.with_extension("json.imported").exists());
        assert!(store.load_record().await.unwrap().steps["slack/raw"].succeeded);
        assert!(!store.import_legacy_record(root.path()).await.unwrap());
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
