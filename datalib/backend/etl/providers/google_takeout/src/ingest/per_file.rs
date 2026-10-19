//! A feed whose every file holds the whole of its records — a Saved list,
//! a My Maps map, a Play Books book: each changed file is read and its
//! rows replace the ones it held; a file gone takes its rows with it.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result};
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable};
use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::prune;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::{file_checkpoint, fsscan};
use datalib_problems::{Problem, Reason};
use datalib_time::IsoOffsetTimestamp;

use super::db::RawDb;

/// One such feed: its name, its product folder, and which files of it it
/// reads.
pub struct Feed {
    pub name: &'static str,
    pub dir: &'static str,
    pub reads: fn(&str) -> bool,
}

impl Feed {
    fn scope(&self) -> String {
        format!("google_takeout/{}", self.name)
    }
}

/// A row that names the file it came from, in its `file` column.
pub trait FromFile: BulkUpsertable {
    fn file(&self) -> &str;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub written: usize,
    pub removed: usize,
}

/// Read every changed file of `feed` with `parse`. A file that will not
/// read or parse is a `skipped:<feed>` row and is left unstamped, so the
/// next run reads it again; the rest of the feed lands.
pub async fn ingest<T: FromFile>(
    db: &RawDb,
    scan: &fsscan::Scan,
    feed: &Feed,
    found: &RunProblems,
    parse: impl Fn(&fsscan::ScannedFile, &[u8]) -> Result<Vec<T>>,
) -> Result<Counts> {
    let scope = feed.scope();
    let prev = file_checkpoint::load_cursor(db.pool(), &scope).await?;
    let changes = scan.changes_since(&prev);
    let mut rows: Vec<T> = Vec::new();
    let mut read: Vec<&fsscan::ScannedFile> = Vec::new();
    let mut skipped: Vec<SkippedRecord> = Vec::new();
    for f in changes
        .needs_reading_by_path()
        .filter(|f| fsscan::is_under(&f.rel, feed.dir) && (feed.reads)(&f.rel))
    {
        let parsed = std::fs::read(&f.path)
            .with_context(|| format!("read {}", super::shape::shape_rel(&f.rel)))
            .and_then(|bytes| parse(f, &bytes));
        match parsed {
            Ok(r) => {
                rows.extend(r);
                read.push(f);
            }
            Err(e) => skipped.push(SkippedRecord {
                entry: f.rel.clone(),
                problem: Problem::record(
                    Reason::Undeserializable,
                    &format!("{}: {}", super::shape::shape_rel(&f.rel), e.root_cause()),
                ),
            }),
        }
    }

    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db
        .pool()
        .begin()
        .await
        .with_context(|| format!("begin {scope} tx"))?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    let mut counts = Counts {
        written: rows.len(),
        removed: 0,
    };
    for f in &read {
        let keep: HashSet<String> = rows
            .iter()
            .filter(|r| r.file() == f.rel)
            .map(|r| r.id().to_string())
            .collect();
        counts.removed += prune::prune_scope_in_tx(&mut tx, T::TABLE, &[("file", &f.rel)], &keep)
            .await?
            .len();
        file_checkpoint::record_file(&mut tx, &scope, f).await?;
    }
    // A file that is gone takes its rows, unless the whole product is
    // missing: then it was left out of this export, not emptied.
    let read_rels: BTreeSet<&str> = read.iter().map(|f| f.rel.as_str()).collect();
    if super::product_exported(scan, feed.dir) {
        for rel in changes.gone_by_path(&read_rels) {
            if !fsscan::is_under(rel, feed.dir) || !(feed.reads)(rel) {
                continue;
            }
            counts.removed +=
                prune::prune_scope_in_tx(&mut tx, T::TABLE, &[("file", rel)], &HashSet::new())
                    .await?
                    .len();
            file_checkpoint::forget_file(&mut tx, &scope, rel).await?;
        }
    }
    tx.commit()
        .await
        .with_context(|| format!("commit {scope} tx"))?;
    found.skipped(feed.name, skipped);
    Ok(counts)
}

/// The `n`th time `key` has come up in one file, counted from 0, so two
/// entries that say the same thing keep two ids.
#[derive(Default)]
pub struct Occurrences(std::collections::HashMap<String, usize>);

impl Occurrences {
    pub fn next(&mut self, key: &str) -> usize {
        let n = self.0.entry(key.to_string()).or_default();
        let this = *n;
        *n += 1;
        this
    }
}
