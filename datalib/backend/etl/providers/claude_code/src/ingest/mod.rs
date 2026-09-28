//! Claude Code's session store → doltlite: one transcript per `.jsonl`
//! file under the transcripts root, read the way every agent-session
//! source reads its files (`datalib_etl_agent_sessions`).

pub mod parse;
pub mod schema_raw;

use std::path::PathBuf;

use anyhow::{Context, Result};

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_agent_sessions::{read_changed, SessionCounts, SessionTree};

pub use datalib_etl_agent_sessions::FetchSummary;

use self::parse::{agent_id_from_path, parse_transcript, ParsedTranscript};
use self::schema_raw::{full_ddl, RecordRow, TranscriptRow, CURSOR_SCOPE};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// The transcripts root: `~/.claude/projects`, or wherever the
    /// config pointed.
    pub input_path: PathBuf,
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();
    let trees = [SessionTree {
        root: opts.input_path.clone(),
        scope: CURSOR_SCOPE.to_string(),
        rel_prefix: String::new(),
    }];
    let mut transcript_rows: Vec<TranscriptRow> = Vec::new();
    let mut record_rows: Vec<RecordRow> = Vec::new();
    let (summary, read) = read_changed(
        db.pool(),
        &opts.cache,
        &trees,
        &opts.progress,
        "claude_code",
        |rel_path, text| {
            let parsed = parse_transcript(text, rel_path, agent_id_from_path(rel_path))?;
            push_rows(&parsed, &mut transcript_rows, &mut record_rows);
            Some(SessionCounts {
                records: parsed.records.len(),
                malformed_lines: parsed.stats.malformed,
                is_subagent: parsed.agent_id.is_some(),
            })
        },
    )
    .await?;

    let mut tx = db.pool().begin().await.context("begin claude_code tx")?;
    bulk_upsert_entity_in_tx(&mut tx, &transcript_rows).await?;
    bulk_upsert_entity_in_tx(&mut tx, &record_rows).await?;
    read.stamp(&mut tx).await?;
    tx.commit().await.context("commit claude_code tx")?;
    Ok(summary)
}

fn push_rows(
    parsed: &ParsedTranscript,
    transcripts: &mut Vec<TranscriptRow>,
    records: &mut Vec<RecordRow>,
) {
    let transcript_id = parsed.transcript_id();
    let meta = &parsed.meta;
    transcripts.push(TranscriptRow {
        id_and_payload: WirePayload {
            id: transcript_id.clone(),
            payload: serde_json::to_string(meta).expect("meta serializes"),
        },
        session_id: parsed.session_id.clone(),
        agent_id: parsed.agent_id.clone(),
        cwd: meta.cwd.clone(),
        git_branch: meta.git_branch.clone(),
        title: meta.title.clone(),
        started_at: meta.started_at.clone(),
        updated_at: meta.updated_at.clone(),
        rel_path: meta.rel_path.clone(),
    });
    for r in &parsed.records {
        records.push(RecordRow {
            id_and_payload: WirePayload {
                id: r.uuid.clone(),
                payload: r.raw.to_string(),
            },
            transcript_id: transcript_id.clone(),
        });
    }
}
