//! Codex's session store → doltlite: one thread per `.jsonl` rollout
//! under `sessions/` and `archived_sessions/` in the Codex home, read
//! the way every agent-session source reads its files
//! (`datalib_etl_agent_sessions`). Each directory keeps its own
//! checkpoint scope.

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
use datalib_etl_codex_config::SESSION_DIRS;

use self::parse::{parse_rollout, record_id, ParsedRollout};
use self::schema_raw::{cursor_scope, full_ddl, RecordRow, TranscriptRow};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// The Codex home: `~/.codex`, or wherever the config pointed.
    pub input_path: PathBuf,
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();
    let trees: Vec<SessionTree> = SESSION_DIRS
        .iter()
        .map(|dir| SessionTree {
            root: opts.input_path.join(dir),
            scope: cursor_scope(dir),
            rel_prefix: format!("{dir}/"),
        })
        .collect();
    let mut transcript_rows: Vec<TranscriptRow> = Vec::new();
    let mut record_rows: Vec<RecordRow> = Vec::new();
    let (summary, read) = read_changed(
        db.pool(),
        &opts.cache,
        &trees,
        &opts.progress,
        "codex",
        |rel_path, text| {
            let parsed = parse_rollout(text, rel_path)?;
            push_rows(&parsed, &mut transcript_rows, &mut record_rows);
            Some(SessionCounts {
                records: parsed.records.len(),
                malformed_lines: parsed.stats.malformed,
                is_subagent: parsed.meta.parent_thread_id.is_some(),
            })
        },
    )
    .await?;

    let mut tx = db.pool().begin().await.context("begin codex tx")?;
    bulk_upsert_entity_in_tx(&mut tx, &transcript_rows).await?;
    bulk_upsert_entity_in_tx(&mut tx, &record_rows).await?;
    read.stamp(&mut tx).await?;
    tx.commit().await.context("commit codex tx")?;
    Ok(summary)
}

fn push_rows(
    parsed: &ParsedRollout,
    transcripts: &mut Vec<TranscriptRow>,
    records: &mut Vec<RecordRow>,
) {
    let meta = &parsed.meta;
    transcripts.push(TranscriptRow {
        id_and_payload: WirePayload {
            id: parsed.thread_id.clone(),
            payload: serde_json::to_string(meta).expect("meta serializes"),
        },
        parent_thread_id: meta.parent_thread_id.clone(),
        cwd: meta.cwd.clone(),
        git_branch: meta.git_branch.clone(),
        title: meta.title.clone(),
        started_at: meta.started_at.clone(),
        updated_at: meta.updated_at.clone(),
        rel_path: meta.rel_path.clone(),
    });
    for l in &parsed.records {
        records.push(RecordRow {
            id_and_payload: WirePayload {
                id: record_id(&parsed.thread_id, l.line_no),
                payload: l.raw.to_string(),
            },
            transcript_id: parsed.thread_id.clone(),
            line_no: l.line_no,
        });
    }
}
