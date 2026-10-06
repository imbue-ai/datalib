//! One lightroom run: the backups the store does not hold, oldest first,
//! one dated commit each, then the live catalog on top when its files
//! changed. Both are found by `fsscan`, so an unchanged input costs a
//! stat. `INGEST.md` §"A folder of backups" has the rules.

use std::path::Path;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::SqlitePool;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::download_problems::RecordProblem;
use datalib_etl::file_checkpoint::{self, INGESTED_FILES_TABLE};
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, Scan, ScanOptions};
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::scope_config;
use datalib_etl::stop::StopFlag;
use datalib_etl_sqlite_mirror::{MirrorOptions, MirrorStats, KEY_RULE_VERSION};

use super::backups::{self, Backup, LEDGER, LEDGER_DDL};
use super::unpack::{self, is_catalog, is_zip};

/// `scope_config`'s key for the filters the newest commit was mirrored
/// under.
const SCOPE: &str = "backups";

/// `scope_config`'s key for whether HEAD is behind the newest backup the
/// store holds: an older backup was replayed and the newest has not been
/// put back on top yet. Written before each backup's commit and cleared
/// once the newest is HEAD, so a run that fails or stops in between leaves
/// the next one to finish the job.
const HEAD_BEHIND: &str = "head_behind";

/// `file_checkpoint`'s scope for the live catalog's files.
const CATALOG_CURSOR: &str = "lightroom/catalog";

/// What one run reads. At least one of the two is set.
pub struct Inputs<'a> {
    pub backups: Option<&'a Path>,
    /// Mirrored last, on top of the backups.
    pub catalog: Option<&'a Path>,
}

#[derive(Debug, Default)]
pub struct SyncRun {
    pub backups_found: usize,
    pub mirrored: Vec<String>,
    /// The live catalog, when the run mirrored it.
    pub live: Option<MirrorStats>,
    /// The live catalog's files had not changed, and nothing else asked
    /// for it to be mirrored again.
    pub catalog_unchanged: bool,
    /// The last backup mirrored.
    pub last: Option<MirrorStats>,
    pub problems: Vec<RecordProblem>,
    /// The run was asked to stop before it was done.
    pub stopped: bool,
}

impl SyncRun {
    fn could_not_mirror(&mut self, found: &RunProblems, backup: &str, why: String) {
        let problem = RecordProblem::new(LEDGER, backup, why);
        found.records_failed([problem.clone()]);
        self.problems.push(problem);
    }

    pub fn summary(&self) -> String {
        let catalog = if self.live.is_some() {
            " catalog=mirrored"
        } else if self.catalog_unchanged {
            " catalog=unchanged"
        } else {
            ""
        };
        let mut s = format!(
            "backups_found={} backups_mirrored={} refused={}{catalog}",
            self.backups_found,
            self.mirrored.len(),
            self.problems.len()
        );
        if let Some(last) = self.live.as_ref().or(self.last.as_ref()) {
            s.push(' ');
            s.push_str(&last.summary());
        }
        s
    }
}

/// Commits each backup and the live catalog as it mirrors them, and
/// leaves only the problems for the caller's closing commit.
pub async fn run(
    pool: &SqlitePool,
    cache: &FingerprintCache,
    inputs: Inputs<'_>,
    options: &MirrorOptions,
    progress: &Progress,
    stop: &StopFlag,
    label: &str,
) -> Result<SyncRun> {
    run_problems::collecting(pool, stop, |found| async move {
        let run =
            mirror_inputs(pool, cache, inputs, options, progress, stop, label, &found).await?;
        // Every run re-plans every backup, so one that was not stopped
        // tried every backup an earlier run could not mirror.
        found.records_tried_all(LEDGER);
        Ok(run)
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn mirror_inputs(
    pool: &SqlitePool,
    cache: &FingerprintCache,
    inputs: Inputs<'_>,
    options: &MirrorOptions,
    progress: &Progress,
    stop: &StopFlag,
    label: &str,
    found: &RunProblems,
) -> Result<SyncRun> {
    sqlx::query(LEDGER_DDL)
        .execute(pool)
        .await
        .context("create the snapshots ledger")?;
    file_checkpoint::ensure_schema(pool).await?;
    let ledger = backups::read_ledger(pool).await?;

    let mut options = MirrorOptions {
        sidecar_tables: [
            options.sidecar_tables.as_slice(),
            &[LEDGER.to_string(), INGESTED_FILES_TABLE.to_string()],
        ]
        .concat(),
        ..options.clone()
    };
    let scope = scope_of(&options);
    let recorded = scope_config::load(pool, SCOPE).await?;
    let filters_changed = recorded.as_ref().is_some_and(|r| r != &scope);
    let mut head_behind =
        scope_config::load(pool, HEAD_BEHIND).await? == Some(serde_json::json!(true));

    let mut run = SyncRun::default();

    let plan = match inputs.backups {
        Some(dir) => {
            let opts = ScanOptions {
                progress: progress.clone(),
                ..ScanOptions::default()
            };
            let scan = fsscan::scan(cache, dir, &opts, |p| is_zip(p) || is_catalog(p))
                .await
                .with_context(|| format!("scan the backups folder {}", dir.display()))?;
            found.extend(scan.walk_problems_as("backups"));
            let plan = backups::plan(backups::entries(&scan.files), &ledger);
            if plan.found.is_empty() {
                bail!(
                    "found no Lightroom backups in {}: expected folders named like \
                     `2026-09-27 1650`, each holding a .zip or a .lrcat",
                    dir.display()
                );
            }
            plan
        }
        None => backups::Plan::default(),
    };
    run.backups_found = plan.found.len();
    run.problems = plan
        .refused
        .iter()
        .map(|(name, why)| RecordProblem::new(LEDGER, name, why))
        .collect();
    found.records_failed(run.problems.iter().cloned());

    // Backups that would not mirror this run: problems, retried next run
    // since the ledger does not hold them, and never what HEAD ends on.
    let mut failed: Vec<&str> = Vec::new();
    for backup in &plan.ingest {
        if stop.requested() {
            run.stopped = true;
            return Ok(run);
        }
        let Some(stats) = mirror_backup(pool, backup, &options, progress, &mut run, found).await?
        else {
            failed.push(&backup.name);
            continue;
        };
        options.gc = false;
        if inputs.catalog.is_none() {
            set_head_behind(pool, &mut head_behind, true).await?;
        }
        let hash = fsscan::hex(&backup.file.blake3);
        backups::record(pool, &backup.name, backup.taken_at, &backup.file.rel, &hash).await?;
        let msg = format!(
            "download {label}: backup {}\n\n{}",
            backup.file.rel,
            stats.summary()
        );
        // Not announced as a checkpoint: nothing reads this store while
        // the step runs, so the runner has no use for the version.
        let date = backups::commit_date(backup.taken_at);
        dr::commit_run_dated(pool, &msg, date.as_deref()).await?;
        run.mirrored.push(backup.name.clone());
        run.last = Some(stats);
    }
    if stop.requested() {
        run.stopped = true;
        return Ok(run);
    }

    // HEAD has to end on the newest state. With a catalog, that is the
    // catalog, mirrored below. Without one it is the newest backup, which
    // needs mirroring again when a backup older than it was replayed
    // after it — this run or one that did not get as far — or when the
    // filters changed and no new backup carries them. Dated now: the
    // reason is now, not when the backup was taken.
    let again = if inputs.catalog.is_some() {
        None
    } else if !run.mirrored.is_empty() || head_behind {
        Some("to put the newest back on top")
    } else if filters_changed {
        Some("under new filters")
    } else {
        None
    };
    let mut newest_missing = false;
    if let Some(why) = again {
        let newest = plan
            .found
            .iter()
            .map(|b| (b.taken_at, b.name.as_str()))
            .chain(ledger.iter().map(|h| (h.taken_at, h.snapshot.as_str())))
            .filter(|(_, name)| !failed.contains(name))
            .max();
        let last = run.mirrored.last().map(String::as_str);
        match newest {
            Some((_, name)) if Some(name) == last => {}
            Some((_, name)) => match plan.found.iter().find(|b| b.name == name) {
                Some(backup) => {
                    match mirror_backup(pool, backup, &options, progress, &mut run, found).await? {
                        Some(stats) => {
                            let msg = format!(
                                "download {label}: backup {}, mirrored again {why}\n\n{}",
                                backup.file.rel,
                                stats.summary()
                            );
                            dr::commit_run(pool, &msg).await?;
                            run.last = Some(stats);
                        }
                        None => newest_missing = true,
                    }
                }
                None => {
                    newest_missing = true;
                    run.could_not_mirror(
                        found,
                        name,
                        format!(
                            "the newest backup in the store is no longer on disk, so it could \
                             not be mirrored again {why}; HEAD is an older state until the \
                             next backup"
                        ),
                    );
                }
            },
            None => {}
        }
    }

    if inputs.catalog.is_none() {
        set_head_behind(pool, &mut head_behind, newest_missing).await?;
    }

    // Record the filters once HEAD is mirrored under them. An absent
    // record is taken as a match: it is a store from before the record,
    // or a first run.
    if !newest_missing && recorded.as_ref() != Some(&scope) {
        scope_config::store(pool, SCOPE, &scope).await?;
    }

    if let Some(catalog) = inputs.catalog {
        let scan = scan_catalog(cache, catalog).await?;
        found.extend(scan.walk_problems_as("catalog"));
        let changes =
            scan.changes_since(&file_checkpoint::load_cursor(pool, CATALOG_CURSOR).await?);
        // A backup committed this run is HEAD now, so the catalog goes
        // back on top whether or not its files moved.
        let needed = changes.any() || filters_changed || !run.mirrored.is_empty();
        if !needed {
            run.catalog_unchanged = true;
            return Ok(run);
        }
        let stats = unpack::mirror_file(pool, catalog, &options, progress)
            .await
            .with_context(|| format!("mirror the catalog {}", catalog.display()))?;
        for f in changes.needs_reading_by_path() {
            file_checkpoint::record_file_pool(pool, CATALOG_CURSOR, f).await?;
        }
        file_checkpoint::forget_files(pool, CATALOG_CURSOR, &changes.gone()).await?;
        let msg = format!(
            "download {label}: catalog {}\n\n{}",
            catalog.display(),
            stats.summary()
        );
        dr::commit_run(pool, &msg).await?;
        run.live = Some(stats);
    }
    Ok(run)
}

/// `None` for a backup that would not mirror — a zip that will not
/// open, a catalog that is not one — after recording it in `problems`,
/// so one bad backup does not hold up every backup after it.
///
/// That holds only while the failure left the working set as it found
/// it. Past the point where the engine empties the mirror, the working
/// set is half a catalog, and committing the next backup on top of it
/// would publish that; so the step fails, and the next writer's open
/// discards what was left.
async fn mirror_backup(
    pool: &SqlitePool,
    backup: &Backup,
    options: &MirrorOptions,
    progress: &Progress,
    run: &mut SyncRun,
    found: &RunProblems,
) -> Result<Option<MirrorStats>> {
    let before = working_set(pool).await?;
    let err = match unpack::mirror_file(pool, &backup.file.path, options, progress).await {
        Ok(stats) => return Ok(Some(stats)),
        Err(e) => e.context(format!("mirror backup {}", backup.file.rel)),
    };
    if working_set(pool).await? != before {
        return Err(err);
    }
    run.could_not_mirror(found, &backup.name, format!("{err:#}"));
    Ok(None)
}

async fn set_head_behind(pool: &SqlitePool, recorded: &mut bool, behind: bool) -> Result<()> {
    if *recorded != behind {
        scope_config::store(pool, HEAD_BEHIND, &serde_json::json!(behind)).await?;
        *recorded = behind;
    }
    Ok(())
}

/// What is uncommitted, table by table.
async fn working_set(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    sqlx::query_as("SELECT table_name, status FROM dolt_status ORDER BY table_name")
        .fetch_all(pool)
        .await
        .context("read dolt_status")
}

/// The live catalog's own file and its `-wal`, where Lightroom keeps
/// edits it has not yet written back while it runs; a change to either
/// is a change to the catalog. One level deep: `Previews.lrdata` beside
/// it can hold a hundred thousand files.
async fn scan_catalog(cache: &FingerprintCache, catalog: &Path) -> Result<Scan> {
    let dir = catalog
        .parent()
        .with_context(|| format!("{} has no parent folder", catalog.display()))?;
    let name = catalog
        .file_name()
        .with_context(|| format!("{} has no file name", catalog.display()))?
        .to_os_string();
    let mut wal = name.clone();
    wal.push("-wal");
    let opts = ScanOptions {
        max_depth: Some(1),
        ..ScanOptions::default()
    };
    let scan = fsscan::scan(cache, dir, &opts, |p| {
        p.file_name()
            .is_some_and(|n| n == name.as_os_str() || n == wal.as_os_str())
    })
    .await
    .with_context(|| format!("scan {}", catalog.display()))?;
    if !scan.files.iter().any(|f| f.path.file_name() == Some(&name)) {
        bail!("the catalog {} does not exist", catalog.display());
    }
    Ok(scan)
}

/// The options that decide what a mirrored catalog looks like, and the
/// engine's rule for its keys. Not `snapshot` or `gc`: those change how a
/// run reads and stores, not what lands.
fn scope_of(o: &MirrorOptions) -> serde_json::Value {
    serde_json::json!({
        "include_tables": o.include_tables,
        "exclude_tables": o.exclude_tables,
        "exclude_columns": o.exclude_columns,
        "stable_key_columns": o.stable_key_columns,
        "primary_keys": o.primary_keys,
        "key_rule": KEY_RULE_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog's fingerprint is its file and its `-wal`, nothing
    /// else in the folder and nothing under it: a same-named file inside
    /// `Previews.lrdata` is never walked.
    #[tokio::test]
    async fn the_catalog_scan_sees_the_catalog_and_its_wal_only() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("Live.lrcat");
        std::fs::write(&cat, b"catalog").unwrap();
        std::fs::write(d.path().join("Live.lrcat-wal"), b"wal 1").unwrap();
        std::fs::write(d.path().join("Live.lrcat-shm"), b"shm").unwrap();
        std::fs::create_dir(d.path().join("Live Previews.lrdata")).unwrap();
        std::fs::write(d.path().join("Live Previews.lrdata/Live.lrcat"), b"x").unwrap();
        let cache = FingerprintCache::open(&d.path().join("fp.sqlite"))
            .await
            .unwrap();

        let first = scan_catalog(&cache, &cat).await.unwrap();
        let mut rels: Vec<&str> = first.files.iter().map(|f| f.rel.as_str()).collect();
        rels.sort();
        assert_eq!(rels, ["Live.lrcat", "Live.lrcat-wal"]);

        // Lightroom writing to the wal alone is a change.
        std::fs::write(d.path().join("Live.lrcat-wal"), b"wal 2, longer").unwrap();
        let second = scan_catalog(&cache, &cat).await.unwrap();
        let changes = second.changes_since(&first.cursor());
        assert_eq!(changes.modified.len(), 1, "{changes:?}");
        assert_eq!(changes.modified[0].rel, "Live.lrcat-wal");
    }

    #[tokio::test]
    async fn a_missing_catalog_fails_the_scan() {
        let d = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&d.path().join("fp.sqlite"))
            .await
            .unwrap();
        let err = scan_catalog(&cache, &d.path().join("Gone.lrcat"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err:#}");
    }
}
