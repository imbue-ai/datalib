//! Claude Code's session store → doltlite. Walk the transcripts root
//! for `.jsonl` files, re-read every one whose content changed since
//! the last run, and upsert its records. A session file only grows
//! while the session is open, so the cost of a sync is the open
//! sessions; everything else is a `stat`. A transcript that vanishes
//! from disk keeps its rows: Claude Code deletes old sessions on its
//! own schedule, and outliving that is half the point of a mirror.

pub mod parse;
pub mod schema_raw;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::warn;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;

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

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    /// Transcript files the scan saw.
    pub files: usize,
    /// Of those, the ones read this run (new or changed).
    pub files_read: usize,
    pub transcripts: usize,
    pub subagents: usize,
    pub records: usize,
    pub malformed_lines: usize,
    /// Files that parsed as nothing: no line named a session.
    pub not_transcripts: usize,
    pub unreadable: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();

    let scan = fsscan::scan(
        &opts.cache,
        &opts.input_path,
        &fsscan::ScanOptions::default(),
        |p| p.extension().is_some_and(|e| e == "jsonl"),
    )
    .await?;
    let prev = file_checkpoint::load_cursor(db.pool(), CURSOR_SCOPE).await?;
    let changes = scan.changes_since(&prev);

    let mut summary = FetchSummary {
        files: scan.files.len(),
        ..Default::default()
    };
    let mut transcript_rows: Vec<TranscriptRow> = Vec::new();
    let mut record_rows: Vec<RecordRow> = Vec::new();
    let mut done: Vec<&fsscan::ScannedFile> = Vec::new();

    for f in changes.needs_reading() {
        let text = match std::fs::read_to_string(&f.path) {
            Ok(t) => t,
            Err(e) => {
                warn!(event = "claude_code_file_unreadable", path = %f.path.display(), error = %e, "a transcript file could not be read");
                summary.unreadable += 1;
                continue;
            }
        };
        summary.files_read += 1;
        let Some(parsed) = parse_transcript(&text, &f.rel, agent_id_from_path(&f.rel)) else {
            summary.not_transcripts += 1;
            // Stamped as read anyway: a file that names no session will
            // not start naming one, and re-reading it every run tells
            // nobody anything.
            done.push(f);
            continue;
        };
        summary.malformed_lines += parsed.stats.malformed;
        summary.records += parsed.records.len();
        summary.transcripts += 1;
        if parsed.agent_id.is_some() {
            summary.subagents += 1;
        }
        push_rows(&parsed, &mut transcript_rows, &mut record_rows);
        done.push(f);
        opts.progress.set_message(&format!(
            "claude_code: {} transcripts / {} records ({} of {} files read)",
            summary.transcripts, summary.records, summary.files_read, summary.files,
        ));
    }

    let mut tx = db.pool().begin().await.context("begin claude_code tx")?;
    bulk_upsert_entity_in_tx(&mut tx, &transcript_rows).await?;
    bulk_upsert_entity_in_tx(&mut tx, &record_rows).await?;
    for f in &done {
        file_checkpoint::record_file(&mut tx, CURSOR_SCOPE, f).await?;
    }
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
