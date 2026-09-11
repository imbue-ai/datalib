// Background job queue for UI-driven sync. The backend inserts rows here
// in response to `POST /api/sync/jobs`; the `datalib worker` child
// process polls for `pending` rows, executes each as one `datalib-dag`
// run, and updates state. Every state transition is committed via
// `CALL DOLT_COMMIT('-Am', 'sync_job: <id> <state>')` so the full history
// lives in `dolt log` next to the data it produced.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What a job does. The SQL column stays free-form because a store
/// predates this enum; [`JobKind::parse`] returns `None` for anything
/// it does not name, so a reader decides what an unknown kind means
/// rather than being handed a wrong answer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum JobKind {
    /// One `datalib-dag` run over the config. The only kind enqueued
    /// today.
    All,
    /// Historical: the fixed download/ingest/render phases, from before
    /// the DAG runner. Named here so an old row still renders.
    Download,
    Ingest,
    Render,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<JobKind> {
        s.parse().ok()
    }
}

/// A job's lifecycle state. `Pending` becomes `Running` when the worker
/// picks it up, then `Done` or `Failed` at completion; either of the
/// first two becomes `Canceled` when the UI asks to stop it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum JobState {
    /// Enqueued, waiting for the worker.
    Pending,
    /// The worker is driving a `datalib-dag` child for it.
    Running,
    Done,
    Failed,
    /// Written by the HTTP cancel handler *before* the child dies: the
    /// worker polls for this value, SIGTERMs its child on seeing it,
    /// and then writes it again as the terminal state.
    Canceled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<JobState> {
        s.parse().ok()
    }

    /// Whether the job is finished. The two that are not are what the
    /// UI's polling chrome counts as active.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, JobState::Pending | JobState::Running)
    }
}

/// One row in the `sync_jobs` table. UI polls `GET /api/sync/jobs/{id}`
/// to render the Lightroom-style progress chrome; cancel flips `state`
/// to `canceled` and the worker SIGTERMs its child.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "sync_jobs", primary_key = "id")]
pub struct SyncJobRow {
    /// Client-or-backend-generated UUIDv4. Used as the row primary key
    /// and in `dolt log` commit messages.
    #[col(sql = "VARCHAR(36)")]
    pub id: String,
    /// Comma-separated source-step ids this run syncs (the UI's "Sync
    /// now" → `--sync <group>/ingest` per source). NULL/empty = the
    /// whole config. Plural: one job routinely covers several steps.
    #[col(sql = "VARCHAR(64)")]
    pub source_ids: Option<String>,
    /// A [`JobKind`], as its `as_str`.
    #[col(sql = "VARCHAR(16)")]
    pub kind: String,
    /// Historical only: before the DAG runner, `all` jobs enqueued
    /// per-source child rows pointing here. New rows are always NULL;
    /// kept so old rows still render.
    #[col(sql = "VARCHAR(36)")]
    pub parent_job_id: Option<String>,
    /// A [`JobState`], as its `as_str`. Read it back with
    /// [`SyncJobRow::job_state`] rather than comparing strings.
    #[col(sql = "VARCHAR(16)")]
    pub state: String,
    /// When the backend enqueued the row (ISO-8601 with explicit local
    /// offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub created_at: String,
    /// When the worker flipped `state` to [`JobState::Running`]
    /// (ISO-8601 with explicit offset). NULL while still pending.
    #[col(sql = "VARCHAR(40)")]
    pub started_at: Option<String>,
    /// When the worker flipped `state` to a terminal [`JobState`]
    /// (ISO-8601 with explicit offset). NULL while still
    /// pending/running.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at: Option<String>,
    /// Human-readable error message when `state` is
    /// [`JobState::Failed`]. The full
    /// structured log lives in `<root>/state/job-logs/<id>.log`; this
    /// column is just the summary the UI shows in the chrome.
    #[col(sql = "TEXT")]
    pub error: Option<String>,
    /// OS pid of the active child process while the job is
    /// [`JobState::Running`]. On worker startup, any running row whose
    /// pid is no longer alive gets flipped to [`JobState::Failed`]
    /// (state recovery).
    #[col(sql = "INT")]
    pub pid: Option<i64>,
    /// Latest reported progress, 0.0..1.0. May be NULL when the underlying
    /// step can't report a meaningful fraction (e.g. open-ended Slack
    /// history pull). Drives the progress bar in the UI chrome.
    #[col(sql = "DOUBLE")]
    pub progress_pct: Option<f64>,
    /// Latest human-readable progress line (e.g. `downloaded 14/200
    /// conversations`). Shown on hover over the chrome's progress bar so
    /// the user can see what the worker is actually doing — useful for
    /// distinguishing 'slow but working' from 'stuck'.
    #[col(sql = "VARCHAR(512)")]
    pub progress_msg: Option<String>,
}

impl SyncJobRow {
    /// This row's lifecycle state. `None` for a spelling this build
    /// does not know — an old store, or one a newer server wrote.
    pub fn job_state(&self) -> Option<JobState> {
        JobState::parse(&self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    /// The column is written through `as_str` and the SSE frame is
    /// written through serde. Two spellings of one state would surface
    /// as a job that never leaves "running".
    #[test]
    fn as_str_matches_the_serde_spelling() {
        for &v in JobState::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(JobState::parse(v.as_str()), Some(v));
        }
        for &v in JobKind::VARIANTS {
            assert_eq!(JobKind::parse(v.as_str()), Some(v));
        }
    }
}
