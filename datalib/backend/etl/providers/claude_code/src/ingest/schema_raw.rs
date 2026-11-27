//! Raw-store schema for the `claude_code` provider: one row per
//! transcript file and one per record in it. Neither table has a
//! bookkeeping sidecar: a live session's file is re-read whole on every
//! sync, and stamping thousands of unchanged rows each time would churn
//! the store for nothing (the same call `airvisual` and `file_inventory` made).

use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;

pub use datalib_etl_agent_sessions::DATA_TABLES;

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

/// `records` — one row per content-bearing line of a transcript, its id
/// the `uuid` Claude Code gave it. The payload is the line as written.
/// Only `user`, `assistant` and `system` records land here; the rest are
/// bookkeeping and fold into the transcript row.
///
/// `transcript_id` is the one promoted column: the writer composes it
/// (`<session_id>#<agent_id>` for a subagent) and the render's diff
/// groups its render units by it. It leads the key, so a transcript's
/// records are one range of the store, read together rather than by
/// one random read each.
/// Everything else a reader wants — `sessionId`, `type`, `timestamp`,
/// `parentUuid`, `isSidechain` — is in the payload; query it as
/// `payload->>'$.type'`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "records", cluster_by = "transcript_id")]
pub struct RecordRow {
    pub id_and_payload: WirePayload,
    pub transcript_id: String,
}

pub fn full_ddl() -> Vec<String> {
    datalib_etl_agent_sessions::raw_ddl(
        TranscriptRow::ddl(),
        RecordRow::ddl(),
        RecordRow::index_ddls(),
    )
}

/// The store's rungs (`datalib/backend/etl/README.md` § "The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "records are keyed by their transcript",
    apply: |conn| {
        Box::pin(async move { dr::run_statements(&mut *conn, &RECORDS_BY_TRANSCRIPT).await })
    },
}];

/// The records go into a table keyed `(transcript_id, id)` as
/// [`RecordRow`] declares it, then take the old one's name. The rows
/// are copied in key order, so the new tree is written front to back.
const RECORDS_BY_TRANSCRIPT: [dr::Statement; 6] = [
    dr::Statement {
        what: "a records table keyed by transcript",
        sql: "CREATE TABLE records_by_transcript (
    id             TEXT NOT NULL,
    payload        TEXT NULL,
    transcript_id  TEXT NOT NULL,
    PRIMARY KEY (transcript_id, id)
)",
    },
    dr::Statement {
        what: "copying every record into it",
        sql: "INSERT INTO records_by_transcript (id, payload, transcript_id)
     SELECT id, payload, transcript_id FROM records ORDER BY transcript_id, id",
    },
    dr::Statement {
        what: "dropping the old transcript index",
        sql: "DROP INDEX IF EXISTS records_transcript",
    },
    dr::Statement {
        what: "dropping the old records table",
        sql: "DROP TABLE records",
    },
    dr::Statement {
        what: "naming the new table records",
        sql: "ALTER TABLE records_by_transcript RENAME TO records",
    },
    dr::Statement {
        what: "indexing the records by id",
        sql: "CREATE UNIQUE INDEX IF NOT EXISTS records_by_id ON records (id)",
    },
];

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
