//! The `gpx` download side: find the `.gpx` files under a folder, split
//! each changed one into rows, and write the rows that differ.

pub mod db;
pub mod model;
pub mod rename;
pub mod rows;
pub mod schema_raw;
pub mod tree;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::download_problems::{self, RecordProblem};
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, ScannedFile};
use datalib_etl::progress::Progress;

pub use db::{db_path_for, RawDb};
use model::{Fidelity, GpxFile};
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
    /// Files read at a new path that kept the rows of a file gone from
    /// an old one.
    pub renamed: usize,
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

    let mut keys = Keys::new(db::stored_keys(&pool).await?, &changes);
    let mut dropped = db::Dropped::new();
    let mut failed: Vec<RecordProblem> = Vec::new();
    let mut read: BTreeSet<&str> = BTreeSet::new();
    let mut batch: Vec<Pending> = Vec::new();
    let mut batch_points = 0;
    for f in changes.needs_reading_by_path() {
        opts.progress.inc(1);
        let (file, fidelity) = match parse_file(f) {
            Ok(parsed) => parsed,
            Err(e) => {
                s.errors += 1;
                failed.push(RecordProblem::new("gpx_files", &f.rel, format!("{e:#}")));
                continue;
            }
        };
        match fidelity {
            Fidelity::Exact => s.exact += 1,
            Fidelity::Equivalent => s.equivalent += 1,
            Fidelity::Lossy => s.lossy += 1,
        }
        let (file_key, renamed_from) = keys.pick(&pool, f, &file).await?;
        let facts = FileFacts {
            path: &f.rel,
            file_key: &file_key,
            blake3: &fsscan::hex(&f.blake3),
            size: f.size,
            fidelity,
        };
        let rows = rows::to_rows(&facts, &file)?;
        batch_points += rows.points.iter().map(|(_, r)| r.len()).sum::<usize>();
        batch.push(Pending {
            file: f,
            rows,
            renamed_from,
        });
        if batch_points >= BATCH_POINTS {
            write_batch(&pool, &mut batch, &mut dropped, &mut s, &mut read).await?;
            batch_points = 0;
        }
    }
    write_batch(&pool, &mut batch, &mut dropped, &mut s, &mut read).await?;

    // A path whose key a renamed file took is already dealt with: its
    // rows are the new path's now.
    let gone: Vec<&str> = changes
        .gone_by_path(&read)
        .into_iter()
        .filter(|rel| !keys.claimed.contains(*rel))
        .collect();
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

/// Read, split, and measure one file: what it is, and how well its rows
/// give it back.
fn parse_file(f: &ScannedFile) -> Result<(GpxFile, Fidelity)> {
    let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let src = String::from_utf8(bytes).context("the file is not UTF-8")?;
    let doc = tree::parse(&src)?;
    let file = model::split(&doc)?;
    let fidelity = Fidelity::measure(&src, &model::rebuild(&file)?)?;
    Ok((file, fidelity))
}

struct Pending<'a> {
    file: &'a ScannedFile,
    rows: FileRows,
    /// The gone path whose key this file took.
    renamed_from: Option<String>,
}

/// Hands each file read this run the key its rows go under: the one it
/// is stored under, else the one of the file it was renamed from, else a
/// new one. `INGEST.md` §"Renames".
struct Keys<'a> {
    stored: HashMap<String, String>,
    in_use: HashSet<String>,
    /// New path → old path, for files `fsscan` saw move unchanged.
    moved_from: HashMap<&'a str, &'a str>,
    /// Stored paths gone from the tree, in path order, with the point ids
    /// each names once something has needed them.
    vanished: Vec<(&'a str, Option<HashSet<String>>)>,
    /// Old paths whose key a new path took.
    claimed: BTreeSet<String>,
}

impl<'a> Keys<'a> {
    fn new(stored: HashMap<String, String>, changes: &'a fsscan::Changes) -> Self {
        let mut vanished: Vec<(&str, Option<HashSet<String>>)> = changes
            .gone()
            .into_iter()
            .filter(|p| stored.contains_key(*p))
            .map(|p| (p, None))
            .collect();
        vanished.sort_by_key(|(p, _)| *p);
        Keys {
            in_use: stored.values().cloned().collect(),
            moved_from: changes
                .moved
                .iter()
                .map(|m| (m.now.rel.as_str(), m.was.as_str()))
                .collect(),
            stored,
            vanished,
            claimed: BTreeSet::new(),
        }
    }

    async fn pick(
        &mut self,
        pool: &sqlx::SqlitePool,
        f: &ScannedFile,
        file: &GpxFile,
    ) -> Result<(String, Option<String>)> {
        if let Some(key) = self.stored.get(&f.rel) {
            return Ok((key.clone(), None));
        }
        if let Some(was) = self.moved_from.get(f.rel.as_str()) {
            if let Some(key) = self.stored.get(*was) {
                self.claimed.insert(was.to_string());
                return Ok((key.clone(), Some(was.to_string())));
            }
        }
        let ids = file.point_ids();
        if !ids.is_empty()
            && self
                .vanished
                .iter()
                .any(|(p, _)| !self.claimed.contains(*p))
        {
            for (p, loaded) in self.vanished.iter_mut() {
                if loaded.is_none() {
                    *loaded = Some(db::point_ids(pool, &self.stored[*p]).await?);
                }
            }
            let open: Vec<rename::Vanished<'_>> = self
                .vanished
                .iter()
                .filter(|(p, _)| !self.claimed.contains(*p))
                .filter_map(|(p, ids)| {
                    ids.as_ref()
                        .map(|point_ids| rename::Vanished { path: p, point_ids })
                })
                .collect();
            if let Some(i) = rename::heir_of(&ids, &open) {
                let was = open[i].path.to_string();
                self.claimed.insert(was.clone());
                return Ok((self.stored[&was].clone(), Some(was)));
            }
        }
        let key = rename::mint_file_key(&f.rel, &fsscan::hex(&f.blake3), &self.in_use);
        self.in_use.insert(key.clone());
        Ok((key, None))
    }
}

async fn write_batch<'a>(
    pool: &sqlx::SqlitePool,
    batch: &mut Vec<Pending<'a>>,
    dropped: &mut db::Dropped,
    s: &mut FetchSummary,
    read: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await.context("begin write tx")?;
    for p in batch.iter() {
        let w = db::write_file(&mut tx, &p.rows, dropped)
            .await
            .with_context(|| format!("write {}", p.file.rel))?;
        s.points_added += w.points_added;
        // In the transaction that wrote the rows, so a crash cannot leave
        // a stamp claiming rows that never landed, or forget a path whose
        // rows were not yet taken over.
        file_checkpoint::record_file(&mut tx, SCOPE, p.file).await?;
        if let Some(was) = &p.renamed_from {
            file_checkpoint::forget_file(&mut tx, SCOPE, was).await?;
        }
    }
    tx.commit().await.context("commit write tx")?;
    for p in batch.drain(..) {
        s.read += 1;
        s.renamed += usize::from(p.renamed_from.is_some());
        read.insert(p.file.rel.as_str());
    }
    Ok(())
}
