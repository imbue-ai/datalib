// Background job queue for UI-driven sync. The backend inserts rows here
// in response to `POST /api/sync/jobs`; the `datalib worker` child
// process polls for `pending` rows, executes each as one `datalib-dag`
// run, and updates state. Every state transition is committed via
// `CALL DOLT_COMMIT('-Am', 'sync_job: <id> <state>')` so the full history
// lives in `dolt log` next to the data it produced.
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`.

use frankweiler_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

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
    /// Comma-separated `sources[].name` subset this run syncs (the UI's
    /// "Sync selected" checkboxes → `--sync <name>.download` per name).
    /// NULL/empty = the whole config.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: Option<String>,
    /// What the job does. `all` — one `datalib-dag` run over the config
    /// — is the only kind enqueued today; the column stays free-form
    /// because historical rows carry the retired fixed-phase kinds
    /// (`download`, `ingest`, `render`).
    #[col(sql = "VARCHAR(16)")]
    pub kind: String,
    /// Historical only: before the DAG runner, `all` jobs enqueued
    /// per-source child rows pointing here. New rows are always NULL;
    /// kept so old rows still render.
    #[col(sql = "VARCHAR(36)")]
    pub parent_job_id: Option<String>,
    /// Lifecycle state. `pending` → `running` when the worker picks it
    /// up; `running` → `done` / `failed` at completion; `pending` /
    /// `running` → `canceled` when the UI requests a cancel (worker
    /// SIGTERMs the child and moves on).
    #[col(sql = "VARCHAR(16)")]
    pub state: String,
    /// When the backend enqueued the row (ISO-8601 with explicit local
    /// offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub created_at: String,
    /// When the worker flipped `state` to `running` (ISO-8601 with
    /// explicit offset). NULL while still `pending`.
    #[col(sql = "VARCHAR(40)")]
    pub started_at: Option<String>,
    /// When the worker flipped `state` to a terminal value — `done`,
    /// `failed`, or `canceled` (ISO-8601 with explicit offset). NULL
    /// while still pending/running.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at: Option<String>,
    /// Human-readable error message when `state = failed`. The full
    /// structured log lives in `<root>/state/job-logs/<id>.log`; this
    /// column is just the summary the UI shows in the chrome.
    #[col(sql = "TEXT")]
    pub error: Option<String>,
    /// OS pid of the active child process while `state = running`. On
    /// worker startup, any `running` rows whose pid is no longer alive get
    /// flipped to `failed` (state recovery).
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
