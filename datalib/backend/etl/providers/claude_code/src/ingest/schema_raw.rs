//! Raw-store schema for the `claude_code` provider: one row per
//! transcript file and one per record in it. Neither table has a
//! bookkeeping sidecar: a live session's file is re-read whole on every
//! sync, and stamping thousands of unchanged rows each time would churn
//! the store for nothing (the same call `airvisual` and `fsindex` made).

use datalib_etl::doltlite_raw::{WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;

pub const DATA_TABLES: &[&str] = &["transcripts", "records"];

/// The one file-cursor scope this provider owns.
pub const CURSOR_SCOPE: &str = "claude_code/sessions";
pub const CURSOR_SCOPE_PREFIX: &str = "claude_code/";

/// `transcripts` — one row per `.jsonl` file: a session, or one of its
/// subagents. The id is the session id Claude Code minted, or
/// `<session_id>#<agent_id>` for a subagent, whose records carry the
/// parent's `sessionId` and their own `agentId`. The payload is what
/// the file says about itself as a whole: the title records, the
/// cloud-session link, the PR links, the versions and models seen.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "transcripts")]
pub struct TranscriptRow {
    pub id_and_payload: WirePayload,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    /// First and last record timestamps, as Claude Code wrote them
    /// (RFC 3339, UTC).
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    /// Path relative to the scanned root, so a reader can find the file.
    pub rel_path: String,
}

/// `records` — one row per content-bearing line of a transcript, keyed
/// by the `uuid` Claude Code gave it. The payload is the line as
/// written. Only `user`, `assistant` and `system` records land here;
/// the rest are bookkeeping and fold into the transcript row.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "records")]
pub struct RecordRow {
    pub id_and_payload: WirePayload,
    pub transcript_id: String,
    pub session_id: String,
    pub record_type: String,
    pub timestamp: Option<String>,
    pub parent_uuid: Option<String>,
    pub is_sidechain: i64,
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
