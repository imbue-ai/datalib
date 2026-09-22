//! Raw-store schema for the `codex` provider: one row per rollout file
//! and one per line in it. Neither table has a bookkeeping sidecar: a
//! live thread's file is re-read whole on every sync, and stamping
//! thousands of unchanged rows each time would churn the store for
//! nothing (the same call `claude_code` made).

use datalib_etl::doltlite_raw::{WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;

pub const DATA_TABLES: &[&str] = &["transcripts", "records"];

/// The file-cursor scopes this provider owns: one per directory under
/// the Codex home that holds rollouts.
pub const CURSOR_SCOPE_PREFIX: &str = "codex/";

pub fn cursor_scope(dir: &str) -> String {
    format!("{CURSOR_SCOPE_PREFIX}{dir}")
}

/// `transcripts` — one row per rollout file, which is one thread: a
/// session, or a sub-agent thread Codex spawned from one. The id is the
/// thread id Codex minted (a UUIDv7). The payload is what the file says
/// about itself as a whole: where it ran, which models spoke, the
/// parent thread, the counts by line type.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "transcripts")]
pub struct TranscriptRow {
    pub id_and_payload: WirePayload,
    /// The thread this one was spawned from, for a sub-agent thread.
    pub parent_thread_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    /// First and last line timestamps, as Codex wrote them (RFC 3339,
    /// UTC).
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    /// Path relative to the Codex home, so a reader can find the file.
    pub rel_path: String,
}

/// `records` — one row per line of a rollout, keyed `<thread_id>#<line>`
/// because Codex gives a line no id of its own; a rollout only ever
/// grows, so a line's number is as stable as its content. The payload
/// is the line as written: `session_meta`, `turn_context`,
/// `response_item`, `event_msg`, `compacted`. Query the type as
/// `payload->>'$.type'` and the item's as `payload->>'$.payload.type'`.
///
/// `transcript_id` and `line_no` are the promoted columns: the render's
/// diff buckets on the first and orders on the second.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "records")]
pub struct RecordRow {
    pub id_and_payload: WirePayload,
    pub transcript_id: String,
    pub line_no: i64,
}

pub const RECORDS_TRANSCRIPT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS records_transcript ON records(transcript_id)";

pub fn full_ddl() -> Vec<String> {
    vec![
        TranscriptRow::ddl(),
        RecordRow::ddl(),
        RECORDS_TRANSCRIPT_INDEX_DDL.to_string(),
        datalib_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_ddl_covers_every_table_and_no_sidecar() {
        let blob = full_ddl().join("\n");
        for t in DATA_TABLES {
            assert!(blob.contains(t), "missing DDL for {t}");
        }
        assert!(!blob.contains("_bookkeeping"));
        assert!(blob.contains("ingested_files"));
    }
}
