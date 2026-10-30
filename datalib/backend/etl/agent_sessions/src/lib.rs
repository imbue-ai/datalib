//! The download every agent-session source shares — claude_code and
//! codex. An agent keeps one `.jsonl` file per session and appends to
//! it while the session is open, so a sync re-reads the files whose
//! content changed since the last one and `stat`s the rest. A file that
//! vanishes keeps its rows: the agent deletes old sessions on its own
//! schedule, and outliving that is half the point of a mirror.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use datalib_problems::{Outcome, Problem, Reason};
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};

use datalib_etl::bulk::{bulk_upsert_entity_in_tx, BulkUpsertable};
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_files::fsscan::{self, ScannedFile};

/// The two tables every agent-session raw store keeps: `transcripts`,
/// one row per session file, and `records`, one per line it keeps from
/// that file, which a render's diff buckets on `records.transcript_id`.
pub const DATA_TABLES: &[&str] = &["transcripts", "records"];

/// An agent-session raw store's DDL, given its two tables' own.
pub fn raw_ddl(transcripts: String, records: String) -> Vec<String> {
    vec![
        transcripts,
        records,
        "CREATE INDEX IF NOT EXISTS records_transcript ON records(transcript_id)".to_string(),
        file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ]
}

/// One directory of session files, and the checkpoint scope its reads
/// are stamped under.
pub struct SessionTree {
    pub root: PathBuf,
    pub scope: String,
    /// Put in front of a file's path under `root` to make the path the
    /// rows keep: `""` when there is one tree, the directory's name when
    /// there are several.
    pub rel_prefix: String,
    /// Whether the agent may simply not have made this directory, so its
    /// absence is nothing to report.
    pub optional: bool,
}

/// What one session file held, as far as the run's summary cares.
pub struct SessionCounts {
    pub records: usize,
    pub malformed_lines: usize,
    pub is_subagent: bool,
    /// [`SkippedLines::summary`]: what the file's problem row says.
    pub skipped: Option<String>,
}

/// The lines of one session file a parser stepped over.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkippedLines {
    count: usize,
    first: Option<(usize, &'static str)>,
    last: Option<usize>,
}

impl SkippedLines {
    pub fn skip(&mut self, line_no: usize, why: &'static str) {
        self.count += 1;
        self.first.get_or_insert((line_no, why));
        self.last = Some(line_no);
    }

    /// `None` when nothing but the file's last line was skipped: a
    /// session open right now has a half-written last line, and the read
    /// after the agent finishes it keeps it.
    pub fn summary(&self, last_line_no: usize) -> Option<String> {
        let torn = usize::from(self.last == Some(last_line_no));
        let count = self.count - torn;
        let (line, why) = self.first.filter(|_| count > 0)?;
        let noun = if count == 1 { "line" } else { "lines" };
        Some(format!(
            "{count} {noun} could not be used; first: line {line}, {why}"
        ))
    }
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

type FileProblem = (Outcome, Problem);

/// One session file's rows, as a provider's parser makes them.
pub struct ParsedSession<T, R> {
    pub transcript: T,
    pub records: Vec<R>,
    pub counts: SessionCounts,
}

/// A file the scan says changed, and where its rows and stamp go.
struct ToRead {
    scope: String,
    rel_path: String,
    file: ScannedFile,
}

/// Whether a path under one tree is one this run had no way to read.
type Unseen = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// File bytes read between writes. Rows are written as the files are
/// read, so the step's count of files moves through the write too and a
/// run holds one batch of rows rather than the whole tree's.
const BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Rows read and not yet written, and how many files they came from.
struct Batch<T, R> {
    transcripts: Vec<T>,
    records: Vec<R>,
    files: u64,
    bytes: u64,
}

impl<T: BulkUpsertable, R: BulkUpsertable> Batch<T, R> {
    fn new() -> Self {
        Self {
            transcripts: Vec::new(),
            records: Vec::new(),
            files: 0,
            bytes: 0,
        }
    }

    async fn write(&mut self, tx: &mut Transaction<'_, Sqlite>, bar: &RunBar) -> Result<()> {
        bulk_upsert_entity_in_tx(tx, &self.transcripts).await?;
        bulk_upsert_entity_in_tx(tx, &self.records).await?;
        bar.did(self.files);
        *self = Self::new();
        Ok(())
    }
}

/// What a file's stamp says it could not use: lines its parser stepped
/// over, and bytes that were not UTF-8, read as U+FFFD so one stray byte
/// costs its line's text rather than the whole file.
fn file_problem(lossy: bool, skipped: Option<String>) -> Option<FileProblem> {
    let outcome = if skipped.is_some() {
        Outcome::Dropped
    } else {
        Outcome::Nulled
    };
    let detail: Vec<String> = lossy
        .then(|| "bytes that are not UTF-8 were replaced".to_string())
        .into_iter()
        .chain(skipped)
        .collect();
    (!detail.is_empty()).then(|| {
        (
            outcome,
            Problem::record(Reason::Undeserializable, &detail.join("; ")),
        )
    })
}

/// Read every changed `.jsonl` under `trees`, hand each to `parse`
/// with the path its rows keep, and write the rows it makes. `parse`
/// returns `None` when the file is not a session.
///
/// A file that could not be read is left unstamped, so the next run
/// tries it again. One that was read is stamped whether or not it was a
/// session: a file that names no session will not start naming one.
/// Every row and stamp is written in one transaction.
///
/// A tree that is not a directory fails the run only when nothing has
/// been read from any tree yet; otherwise what is stored stands and the
/// tree is a `listing:` problem.
///
/// Every run walks every tree and re-reads every file it could not read
/// before, so an unreadable file's row stands only where this run could
/// not look: under an entry a walk could not read, or a tree that is gone.
pub async fn read_changed<T: BulkUpsertable, R: BulkUpsertable>(
    pool: &SqlitePool,
    cache: &FingerprintCache,
    trees: &[SessionTree],
    progress: &Progress,
    provider: &str,
    found: &RunProblems,
    mut parse: impl FnMut(&str, &str) -> Option<ParsedSession<T, R>>,
) -> Result<FetchSummary> {
    let mut summary = FetchSummary::default();
    let mut to_read: Vec<ToRead> = Vec::new();
    let mut gone: Vec<(String, String)> = Vec::new();
    let mut unseen: Vec<(String, Unseen)> = Vec::new();
    let mut missing = Vec::new();
    let mut stored = 0;
    for tree in trees {
        if !tree.root.is_dir() && tree.optional {
            continue;
        }
        let prev = file_checkpoint::load_cursor(pool, &tree.scope).await?;
        stored += prev.len();
        if !tree.root.is_dir() {
            missing.push(tree);
            continue;
        }
        let scan = fsscan::scan(cache, &tree.root, &fsscan::ScanOptions::default(), |p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })
        .await?;
        scan.report_problems(found, &tree.scope);
        if !scan.errors.is_empty() {
            unseen.push((tree.rel_prefix.clone(), Box::new(scan.unseen())));
        }
        let changes = scan.changes_since(&prev);
        summary.files += scan.files.len();
        gone.extend(
            changes
                .gone()
                .into_iter()
                .map(|rel| (tree.scope.clone(), rel.to_string())),
        );
        to_read.extend(changes.needs_reading().map(|f| ToRead {
            scope: tree.scope.clone(),
            rel_path: format!("{}{}", tree.rel_prefix, f.rel),
            file: f.clone(),
        }));
    }
    if let Some(first) = missing.first() {
        if stored == 0 && summary.files == 0 {
            bail!("{} is not a directory", first.root.display());
        }
    }

    let bar = RunBar::new(progress, to_read.len() as u64);
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin {provider} tx"))?;
    let mut batch = Batch::<T, R>::new();
    for ToRead {
        scope,
        rel_path,
        file,
    } in to_read
    {
        batch.files += 1;
        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            Err(e) => {
                summary.unreadable += 1;
                found.record_failed("transcripts", &rel_path, e.to_string());
                continue;
            }
        };
        batch.bytes += bytes.len() as u64;
        let text = String::from_utf8_lossy(&bytes);
        let lossy = matches!(text, std::borrow::Cow::Owned(_));
        summary.files_read += 1;
        let problem = match parse(&rel_path, &text) {
            None => {
                summary.not_transcripts += 1;
                None
            }
            Some(session) => {
                let counts = session.counts;
                summary.transcripts += 1;
                summary.records += counts.records;
                summary.malformed_lines += counts.malformed_lines;
                if counts.is_subagent {
                    summary.subagents += 1;
                }
                batch.transcripts.push(session.transcript);
                batch.records.extend(session.records);
                file_problem(lossy, counts.skipped)
            }
        };
        file_checkpoint::record_file_with_problem(&mut tx, &scope, &file, problem).await?;
        bar.doing(&format!(
            "{provider}: {} transcripts / {} records ({} of {} files read)",
            summary.transcripts, summary.records, summary.files_read, summary.files,
        ));
        if batch.bytes >= BATCH_BYTES {
            batch.write(&mut tx, &bar).await?;
        }
    }
    batch.write(&mut tx, &bar).await?;
    for (scope, rel) in &gone {
        file_checkpoint::forget_file(&mut tx, scope, rel).await?;
    }
    tx.commit()
        .await
        .with_context(|| format!("commit {provider} tx"))?;

    for tree in missing {
        unseen.push((tree.rel_prefix.clone(), Box::new(|_| true)));
        found.listing(
            &tree.scope,
            format!(
                "{} is not a directory; what was read from it before is kept",
                tree.root.display()
            ),
        );
    }
    found.records_tried_all_but("transcripts", move |id| {
        unseen
            .iter()
            .any(|(rel_prefix, unseen)| id.strip_prefix(rel_prefix.as_str()).is_some_and(unseen))
    });
    Ok(summary)
}
