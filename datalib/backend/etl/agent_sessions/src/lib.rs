//! The download every agent-session source shares — claude_code and
//! codex. An agent keeps one `.jsonl` file per session and appends to
//! it while the session is open, so a sync re-reads the files whose
//! content changed since the last one and `stat`s the rest. A file that
//! vanishes keeps its rows: the agent deletes old sessions on its own
//! schedule, and outliving that is half the point of a mirror.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tracing::warn;

use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, ScannedFile};
use datalib_etl::progress::Progress;

/// One directory of session files, and the checkpoint scope its reads
/// are stamped under.
pub struct SessionTree {
    pub root: PathBuf,
    pub scope: String,
    /// Put in front of a file's path under `root` to make the path the
    /// rows keep: `""` when there is one tree, the directory's name when
    /// there are several.
    pub rel_prefix: String,
}

/// What one session file held, as far as the run's summary cares.
pub struct SessionCounts {
    pub records: usize,
    pub malformed_lines: usize,
    pub is_subagent: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    /// Session files the scan saw.
    pub files: usize,
    /// Of those, the ones read this run (new or changed).
    pub files_read: usize,
    /// Files that parsed as a session, or a subagent's part of one.
    pub transcripts: usize,
    pub subagents: usize,
    pub records: usize,
    pub malformed_lines: usize,
    /// Files that parsed as nothing: nothing in them named a session.
    pub not_transcripts: usize,
    pub unreadable: usize,
}

impl FetchSummary {
    /// The step's one-line run summary.
    pub fn line(&self) -> String {
        format!(
            "files={} read={} transcripts={} subagents={} records={} malformed_lines={} \
             not_transcripts={} unreadable={}",
            self.files,
            self.files_read,
            self.transcripts,
            self.subagents,
            self.records,
            self.malformed_lines,
            self.not_transcripts,
            self.unreadable,
        )
    }
}

/// The files a run read, to stamp as read in the transaction that
/// writes their rows.
pub struct ReadFiles(Vec<(String, ScannedFile)>);

impl ReadFiles {
    pub async fn stamp(&self, tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
        for (scope, f) in &self.0 {
            file_checkpoint::record_file(tx, scope, f).await?;
        }
        Ok(())
    }
}

/// Hand every changed `.jsonl` under `trees` to `read`, with the path the
/// rows keep and its text. `read` parses the file and keeps its rows,
/// and returns `None` when the file is not a session.
///
/// A file that could not be read is left unstamped, so the next run
/// tries it again. One that was read is stamped whether or not it was a
/// session: a file that names no session will not start naming one.
pub async fn read_changed(
    pool: &SqlitePool,
    cache: &FingerprintCache,
    trees: &[SessionTree],
    progress: &Progress,
    provider: &str,
    mut read: impl FnMut(&str, &str) -> Option<SessionCounts>,
) -> Result<(FetchSummary, ReadFiles)> {
    let mut summary = FetchSummary::default();
    let mut done = Vec::new();
    for tree in trees {
        if !tree.root.is_dir() {
            continue;
        }
        let scan = fsscan::scan(cache, &tree.root, &fsscan::ScanOptions::default(), |p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })
        .await?;
        let prev = file_checkpoint::load_cursor(pool, &tree.scope).await?;
        let changes = scan.changes_since(&prev);
        summary.files += scan.files.len();

        for f in changes.needs_reading() {
            let text = match std::fs::read_to_string(&f.path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(event = "session_file_unreadable", provider, path = %f.path.display(), error = %e, "a session file could not be read");
                    summary.unreadable += 1;
                    continue;
                }
            };
            summary.files_read += 1;
            done.push((tree.scope.clone(), f.clone()));
            let rel_path = format!("{}{}", tree.rel_prefix, f.rel);
            let Some(counts) = read(&rel_path, &text) else {
                summary.not_transcripts += 1;
                continue;
            };
            summary.transcripts += 1;
            summary.records += counts.records;
            summary.malformed_lines += counts.malformed_lines;
            if counts.is_subagent {
                summary.subagents += 1;
            }
            progress.set_message(&format!(
                "{provider}: {} transcripts / {} records ({} of {} files read)",
                summary.transcripts, summary.records, summary.files_read, summary.files,
            ));
        }
    }
    Ok((summary, ReadFiles(done)))
}
