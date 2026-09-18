//! Claude Code's session store → doltlite. Walk the transcripts root
//! for `.jsonl` files, re-read every one whose content changed since
//! the last run, and upsert its records. A session file only grows
//! while the session is open, so the cost of a sync is the open
//! sessions; everything else is a `stat`. A transcript that vanishes
//! from disk keeps its rows: Claude Code deletes old sessions on its
//! own schedule, and outliving that is half the point of a mirror.

pub mod parse;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use tracing::warn;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;

use self::parse::{agent_id_from_path, parse_transcript, ParsedTranscript};
use self::schema_raw::{
    full_ddl, RecordRow, TranscriptRow, CURSOR_SCOPE, CURSOR_SCOPE_PREFIX, DATA_TABLES,
};

pub use datalib_etl::doltlite_raw::db_path_for;

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// The commit a reader is pinned at; `None` for the writer.
    pin: Option<datalib_etl::pin::Pin>,
}

impl RawDb {
    /// Open this store to *read* it, for the render pass: no DDL, no
    /// commits. See `datalib_etl::doltlite_raw::open_reader`.
    /// Pinned at `commit`, else HEAD; `None` when nothing is committed.
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let Some(reader) = dr::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            pool: reader.pool().clone(),
            pin: Some(reader.pin().clone()),
        }))
    }

    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool, pin: None })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The commit this reader reads at. `None` on the writer's handle.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
    }

    pub async fn close(self) {
        self.close_all().await;
    }

    /// `--reset-and-redownload`: empty every table and forget which
    /// files were read.
    pub async fn reset(&self) -> Result<()> {
        for table in DATA_TABLES {
            // Audited: `table` iterates a `&'static str` const array of our own
            // table names; no runtime data reaches the statement.
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&self.pool)
                .await?;
        }
        file_checkpoint::clear_scope_prefix(&self.pool, CURSOR_SCOPE_PREFIX)
            .await
            .context("clear claude_code file cursors on reset")
    }
}

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
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }

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
                warn!(event = "claude_code_file_unreadable", path = %f.path.display(), error = %e);
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
            session_id: parsed.session_id.clone(),
            record_type: r.record_type.clone(),
            timestamp: r.timestamp.clone(),
            parent_uuid: r.parent_uuid.clone(),
            is_sidechain: i64::from(r.is_sidechain),
        });
    }
}
