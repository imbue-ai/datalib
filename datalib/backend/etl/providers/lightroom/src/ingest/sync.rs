//! One lightroom run: the backups the store does not hold, oldest first,
//! one dated commit each, then the newest state on top: the live catalog
//! when there is one, else the newest backup, mirrored again every run so
//! HEAD always ends there under the filters the run was given.
//! `INGEST.md` §"A folder of backups" has the rules.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::SqlitePool;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::download_problems::RecordProblem;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_files::fsscan::{self, ScanOptions};
use datalib_etl_sqlite_mirror::{MirrorOptions, MirrorStats};

use super::backups::{self, Backup, Held, Plan, LEDGER, LEDGER_DDL};
use super::unpack::{self, is_catalog, is_zip};

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
    /// The live catalog, mirrored as the run's last commit.
    pub live: Option<MirrorStats>,
    /// The last backup mirrored: the newest, put back on top, when there
    /// is no live catalog.
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
    let ledger = backups::read_ledger(pool).await?;

    let mut options = MirrorOptions {
        sidecar_tables: [options.sidecar_tables.as_slice(), &[LEDGER.to_string()]].concat(),
        ..options.clone()
    };
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
            scan.report_problems(found, "backups");
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
        None => Plan::default(),
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

    // HEAD ends on the newest state, mirrored under this run's filters.
    // When it already is, the mirror changes nothing and commits nothing.
    if let Some(catalog) = inputs.catalog {
        let stats = unpack::mirror_file(pool, catalog, &options, progress)
            .await
            .with_context(|| format!("mirror the catalog {}", catalog.display()))?;
        let msg = format!(
            "download {label}: catalog {}\n\n{}",
            catalog.display(),
            stats.summary()
        );
        dr::commit_run(pool, &msg).await?;
        run.live = Some(stats);
        return Ok(run);
    }
    match newest(&plan, &ledger, &failed) {
        Newest::Here(backup) => {
            let Some(stats) =
                mirror_backup(pool, backup, &options, progress, &mut run, found).await?
            else {
                return Ok(run);
            };
            // Dated now: the reason for this commit is now, not when the
            // backup was taken.
            let msg = format!(
                "download {label}: backup {}, mirrored again to put the newest back on top\n\n{}",
                backup.file.rel,
                stats.summary()
            );
            dr::commit_run(pool, &msg).await?;
            run.last = Some(stats);
        }
        // Mirroring an older backup on top would take the newest state
        // out of HEAD because its file went, so HEAD stays as it is.
        Newest::Gone(name) => run.could_not_mirror(
            found,
            &name,
            "the newest backup the store holds is no longer in the folder, so it cannot \
             be mirrored again; HEAD stays on the state the last sync left until a newer \
             backup arrives"
                .to_string(),
        ),
        Newest::Nothing => {}
    }
    Ok(run)
}

/// What HEAD should end on when there is no live catalog.
enum Newest<'a> {
    Here(&'a Backup),
    /// The newest backup the store holds, whose bytes are no longer in
    /// the folder.
    Gone(String),
    /// No backup in the folder mirrors, and the store holds none.
    Nothing,
}

/// The newest backup, among those in the folder that did not fail this
/// run and those the store holds. One the store holds is in the folder
/// when its bytes are, whatever its folder is called now.
fn newest<'a>(plan: &'a Plan, ledger: &[Held], failed: &[&str]) -> Newest<'a> {
    let here = plan
        .found
        .iter()
        .filter(|b| !failed.contains(&b.name.as_str()))
        .max_by_key(|b| (b.taken_at, &b.name));
    let on_disk: HashSet<String> = plan
        .found
        .iter()
        .map(|b| fsscan::hex(&b.file.blake3))
        .collect();
    let gone = ledger
        .iter()
        .filter(|h| !on_disk.contains(&h.blake3) && !failed.contains(&h.snapshot.as_str()))
        .max_by_key(|h| h.taken_at);
    match (here, gone) {
        (Some(b), Some(g)) if g.taken_at > b.taken_at => Newest::Gone(g.snapshot.clone()),
        (None, Some(g)) => Newest::Gone(g.snapshot.clone()),
        (Some(b), _) => Newest::Here(b),
        (None, None) => Newest::Nothing,
    }
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

/// What is uncommitted, table by table.
async fn working_set(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    sqlx::query_as("SELECT table_name, status FROM dolt_status ORDER BY table_name")
        .fetch_all(pool)
        .await
        .context("read dolt_status")
}
