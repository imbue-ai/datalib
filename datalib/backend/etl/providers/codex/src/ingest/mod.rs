//! Codex's session store → doltlite. Walk `sessions/` (and
//! `archived_sessions/`, where an older Codex moved a thread on
//! archive) under the Codex home for `.jsonl` rollouts, re-read every
//! one whose content changed since the last run, and upsert its lines.
//! A rollout only grows while its thread is open, so the cost of a sync
//! is the open threads; everything else is a `stat`. A rollout that
//! vanishes from disk keeps its rows: outliving Codex's own housekeeping
//! is half the point of a mirror.

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

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    /// Rollout files the scan saw.
    pub files: usize,
    /// Of those, the ones read this run (new or changed).
    pub files_read: usize,
    pub threads: usize,
    /// Of the threads, the ones spawned from another.
    pub subagents: usize,
    pub records: usize,
    pub malformed_lines: usize,
    /// Files that parsed as nothing: no `session_meta` line.
    pub not_transcripts: usize,
    pub unreadable: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();
    let mut summary = FetchSummary::default();
    let mut transcript_rows: Vec<TranscriptRow> = Vec::new();
    let mut record_rows: Vec<RecordRow> = Vec::new();
    // (cursor scope, file) for every file to stamp as read.
    let mut done: Vec<(String, fsscan::ScannedFile)> = Vec::new();

    for dir in SESSION_DIRS {
        let root = opts.input_path.join(dir);
        if !root.is_dir() {
            continue;
        }
        let scope = cursor_scope(dir);
        let scan = fsscan::scan(&opts.cache, &root, &fsscan::ScanOptions::default(), |p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })
        .await?;
        let prev = file_checkpoint::load_cursor(db.pool(), &scope).await?;
        let changes = scan.changes_since(&prev);
        summary.files += scan.files.len();

        for f in changes.needs_reading() {
            let text = match std::fs::read_to_string(&f.path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(event = "codex_file_unreadable", path = %f.path.display(), error = %e);
                    summary.unreadable += 1;
                    continue;
                }
            };
            summary.files_read += 1;
            let rel_path = format!("{dir}/{}", f.rel);
            let Some(parsed) = parse_rollout(&text, &rel_path) else {
                summary.not_transcripts += 1;
                // Stamped as read anyway: a file with no session_meta
                // will not grow one, and re-reading it every run tells
                // nobody anything.
                done.push((scope.clone(), f.clone()));
                continue;
            };
            summary.malformed_lines += parsed.stats.malformed;
            summary.records += parsed.records.len();
            summary.threads += 1;
            if parsed.meta.parent_thread_id.is_some() {
                summary.subagents += 1;
            }
            push_rows(&parsed, &mut transcript_rows, &mut record_rows);
            done.push((scope.clone(), f.clone()));
            opts.progress.set_message(&format!(
                "codex: {} threads / {} records ({} of {} files read)",
                summary.threads, summary.records, summary.files_read, summary.files,
            ));
        }
    }

    let mut tx = db.pool().begin().await.context("begin codex tx")?;
    bulk_upsert_entity_in_tx(&mut tx, &transcript_rows).await?;
    bulk_upsert_entity_in_tx(&mut tx, &record_rows).await?;
    for (scope, f) in &done {
        file_checkpoint::record_file(&mut tx, scope, f).await?;
    }
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
