//! The `gpx` download side: find the `.gpx` files under a folder, split
//! each changed one into rows, and write the rows that differ.

pub mod db;
pub mod model;
pub mod rows;
pub mod schema_raw;
pub mod tree;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::download_problems::{self, RecordProblem};
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, ScannedFile};
use datalib_etl::progress::Progress;

pub use db::{db_path_for, RawDb};
use model::Fidelity;
use rows::{FileFacts, FileRows};

const SCOPE: &str = "gpx/files";

/// Parsed files are written in one transaction until they hold this many
/// points between them: few enough to keep memory flat, enough that the
/// shared point tables are not rewritten once per small file.
const BATCH_POINTS: usize = 50_000;

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub root: PathBuf,
    pub ignore: Vec<String>,
    /// This host's shared fingerprint cache, so an unchanged file costs a
    /// `stat`.
    pub cache: FingerprintCache,
    pub progress: Progress,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub files: usize,
    pub unchanged: usize,
    pub read: usize,
    pub removed: usize,
    pub exact: usize,
    pub equivalent: usize,
    pub lossy: usize,
    pub points_added: u64,
    pub points_removed: u64,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let pool = opts.db.pool().clone();
    let scan = fsscan::scan(
        &opts.cache,
        &opts.root,
        &fsscan::ScanOptions {
            ignore: opts.ignore.clone(),
            progress: opts.progress.clone(),
            ..Default::default()
        },
        is_gpx,
    )
    .await?;
    let mut s = FetchSummary {
        files: scan.files.len(),
        errors: scan.errors.len(),
        ..FetchSummary::default()
    };
    for e in &scan.errors {
        tracing::warn!(path = %e.path.display(), error = %e.error, "gpx_walk_error");
    }

    let prev = file_checkpoint::load_cursor(&pool, SCOPE).await?;
    let changes = scan.changes_since(&prev);
    s.unchanged = changes.unchanged;
    opts.progress.set_length(Some(scan.files.len() as u64));
    opts.progress.inc(changes.unchanged as u64);

    let mut dropped = db::Dropped::new();
    let mut failed: Vec<RecordProblem> = Vec::new();
    let mut read: BTreeSet<&str> = BTreeSet::new();
    let mut batch: Vec<(&ScannedFile, FileRows)> = Vec::new();
    let mut batch_points = 0;
    for f in changes.needs_reading_by_path() {
        opts.progress.inc(1);
        match parse_file(f) {
            Ok((file_rows, fidelity)) => {
                match fidelity {
                    Fidelity::Exact => s.exact += 1,
                    Fidelity::Equivalent => s.equivalent += 1,
                    Fidelity::Lossy => s.lossy += 1,
                }
                batch_points += file_rows.points.iter().map(|(_, r)| r.len()).sum::<usize>();
                batch.push((f, file_rows));
            }
            Err(e) => {
                s.errors += 1;
                failed.push(RecordProblem::new("gpx_files", &f.rel, format!("{e:#}")));
                continue;
            }
        }
        if batch_points >= BATCH_POINTS {
            write_batch(&pool, &mut batch, &mut dropped, &mut s, &mut read).await?;
            batch_points = 0;
        }
    }
    write_batch(&pool, &mut batch, &mut dropped, &mut s, &mut read).await?;

    let gone = changes.gone_by_path(&read);
    if !gone.is_empty() {
        let mut tx = pool.begin().await.context("begin delete tx")?;
        for rel in gone {
            if db::delete_file(&mut tx, rel, &mut dropped).await? {
                s.removed += 1;
            }
            file_checkpoint::forget_file(&mut tx, SCOPE, rel).await?;
        }
        tx.commit().await.context("commit delete tx")?;
    }

    if !dropped.is_empty() {
        let mut tx = pool.begin().await.context("begin sweep tx")?;
        s.points_removed = db::sweep(&mut tx, &dropped).await?;
        tx.commit().await.context("commit sweep tx")?;
    }

    download_problems::report_records(&pool, &failed).await;
    let lossy: Vec<RecordProblem> = db::lossy_paths(&pool)
        .await?
        .iter()
        .map(|p| {
            RecordProblem::new(
                "gpx_files",
                p,
                "written back from the store, this file differs from the original by more \
                 than whitespace, attribute order or comments",
            )
        })
        .collect();
    download_problems::report_lossy(&pool, "gpx_round_trip", &lossy).await;
    download_problems::report_run(&pool, &scan.walk_problems()).await;
    Ok(s)
}

fn is_gpx(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gpx"))
}

/// Read, split, and measure one file: the rows it becomes, and how well
/// they give it back.
fn parse_file(f: &ScannedFile) -> Result<(FileRows, Fidelity)> {
    let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let src = String::from_utf8(bytes).context("the file is not UTF-8")?;
    let doc = tree::parse(&src)?;
    let file = model::split(&doc)?;
    let fidelity = Fidelity::measure(&src, &model::rebuild(&file)?)?;
    let facts = FileFacts {
        path: &f.rel,
        blake3: &fsscan::hex(&f.blake3),
        size: f.size,
        fidelity,
    };
    Ok((rows::to_rows(&facts, &file)?, fidelity))
}

async fn write_batch<'a>(
    pool: &sqlx::SqlitePool,
    batch: &mut Vec<(&'a ScannedFile, FileRows)>,
    dropped: &mut db::Dropped,
    s: &mut FetchSummary,
    read: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await.context("begin write tx")?;
    for (f, file_rows) in batch.iter() {
        let w = db::write_file(&mut tx, file_rows, dropped)
            .await
            .with_context(|| format!("write {}", f.rel))?;
        s.points_added += w.points_added;
        // In the transaction that wrote the rows, so a crash cannot leave
        // a stamp claiming rows that never landed.
        file_checkpoint::record_file(&mut tx, SCOPE, f).await?;
    }
    tx.commit().await.context("commit write tx")?;
    for (f, _) in batch.drain(..) {
        s.read += 1;
        read.insert(f.rel.as_str());
    }
    Ok(())
}
