//! The `markdown` download side: walk a folder, and store each markdown
//! file's text under its path. Turning a note into a page is the render
//! step's job.

pub mod db;
pub mod schema_raw;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_files::file_changes_scan::{self, ReadFile};
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_problems::{Outcome, Problem, Reason};
use datalib_time::RecordStampPrecision;

pub use db::{db_path_for, RawDb};
use schema_raw::{MarkdownFileRow, MarkdownScanMetaRow};

/// The cursor's scope in `ingested_files`.
pub const CHECKPOINT_SCOPE: &str = "markdown/files";

/// The `record:` table a file the scan could not read is reported under,
/// the one `file_changes_scan` reports its own under.
const FILES: &str = "files";

pub struct FetchOptions {
    pub db: RawDb,
    /// The source's id, the `markdown_scan_meta` key.
    pub source_id: String,
    /// The folder to scan.
    pub root: PathBuf,
    /// Gitignore-shaped patterns the walk prunes, hidden folders included.
    pub ignore: Vec<String>,
    pub max_bytes: Option<u64>,
    /// This host's shared fingerprint cache, so an unchanged file costs a
    /// `stat`.
    pub cache: FingerprintCache,
    /// The run-pinned now, as `DATALIB_DAG_NOW` gives it.
    pub now: String,
    pub progress: Progress,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FetchSummary {
    /// Markdown files the walk found and read, or could have.
    pub files_seen: usize,
    /// Notes stored that were not stored before.
    pub added: usize,
    /// Notes whose file now holds other bytes.
    pub changed: usize,
    /// Files whose bytes are what the store already holds.
    pub unchanged: usize,
    /// Notes whose file is gone: deleted, or moved (then `added` at the
    /// new path).
    pub removed: usize,
    /// Files skipped for exceeding `max_bytes`.
    pub too_large: usize,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let pool = opts.db.pool().clone();
    // A scan has no stop to honour: it is a stat per unchanged file, and
    // reads only what changed.
    let never_stops = StopFlag::new();
    run_problems::collecting(&pool, &never_stops, |found| scan_folder(opts, found)).await
}

async fn scan_folder(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    let now = datalib_time::parse_strict(&opts.now)
        .with_context(|| format!("parse the run's now {:?}", opts.now))?;

    let scan = file_changes_scan::list(
        &opts.cache,
        &opts.root,
        &file_changes_scan::ScanOptions {
            ignore: opts.ignore.clone(),
            max_bytes: opts.max_bytes,
            progress: opts.progress.clone(),
            ..Default::default()
        },
        is_markdown,
    )
    .await?;
    db.write_scan_meta(
        &MarkdownScanMetaRow {
            id: opts.source_id.clone(),
            abs_root: scan.given_resolved.to_string_lossy().to_string(),
        },
        &now,
    )
    .await
    .context("record scan root")?;
    scan.report_problems(&found, FILES);

    let mut summary = FetchSummary {
        files_seen: scan.files.len() + scan.present_unread.len(),
        too_large: scan.over_max_bytes.len(),
        errors: scan.errors.len(),
        ..Default::default()
    };
    let prev = file_checkpoint::load_cursor(db.pool(), CHECKPOINT_SCOPE).await?;
    let changes = scan.changes_since(&prev, |_| true);
    opts.progress.set_length(Some(scan.files.len() as u64));
    opts.progress.inc(changes.unchanged() as u64);

    let mut read: BTreeSet<&str> = BTreeSet::new();
    for &f in changes.to_read() {
        opts.progress.inc(1);
        let file = match changes.read_if_changed_by_path(f).await {
            Ok(Some(file)) => file,
            Ok(None) => continue,
            // Not stamped, so the next run reads it again; and while it
            // stands unread no deletion is made, since it may be where a
            // vanished note went.
            Err(e) => {
                summary.errors += 1;
                found.record_failed(FILES, &f.rel, format!("{e:#}"));
                continue;
            }
        };
        let (row, problem) = note_row(&file, modified_at(&f.path));
        if prev.contains_key(&f.rel) {
            summary.changed += 1;
        } else {
            summary.added += 1;
        }
        db.write_file(&row, CHECKPOINT_SCOPE, &file.file, problem, &now)
            .await?;
        read.insert(f.rel.as_str());
    }

    // A file that is gone takes its note with it; a moved one's old path
    // goes once its new one is stored.
    for rel in changes.gone_by_path(&read) {
        db.delete_file(CHECKPOINT_SCOPE, rel).await?;
        summary.removed += 1;
    }
    summary.unchanged = changes.unchanged();
    scan.finish().await?;
    Ok(summary)
}

/// A file is a note when its name ends `.md` or `.markdown`, in any case.
fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// One read file as its row, and what the read could not keep: bytes
/// that are not UTF-8 are replaced, and the file says so until it is read
/// again.
fn note_row(
    file: &ReadFile,
    modified_at: Option<String>,
) -> (MarkdownFileRow, Option<(Outcome, Problem)>) {
    let (text, problem) = match String::from_utf8(file.bytes.clone()) {
        Ok(text) => (text, None),
        Err(e) => {
            let text = String::from_utf8_lossy(e.as_bytes()).into_owned();
            let problem = Problem::field(
                "text",
                Reason::CoercionFailed,
                &format!(
                    "not UTF-8 from byte {}: each byte that is not is stored as U+FFFD",
                    e.utf8_error().valid_up_to()
                ),
            );
            (text, Some((Outcome::Nulled, problem)))
        }
    };
    let row = MarkdownFileRow {
        id: file.file.rel.clone(),
        blake3: file_changes_scan::hex(&file.file.blake3),
        size: file.file.size,
        modified_at,
        text,
    };
    (row, problem)
}

/// The file's mtime, which names no clock: `Z`.
fn modified_at(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let ms = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    datalib_time::record_stamp_from_unix_millis_at(
        Some(i64::try_from(ms).ok()?),
        None,
        RecordStampPrecision::Seconds,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_extensions_match_in_any_case() {
        for yes in ["a.md", "a.MD", "Log.Markdown", "dir/x.markdown"] {
            assert!(is_markdown(Path::new(yes)), "{yes}");
        }
        for no in ["a.txt", "md", "a.mdx", "a.md.bak"] {
            assert!(!is_markdown(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn the_mtime_of_a_file_names_no_clock() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log.md");
        std::fs::write(&p, "x").unwrap();
        let stamp = modified_at(&p).unwrap();
        assert!(stamp.ends_with('Z'), "{stamp}");
    }
}
