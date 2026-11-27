//! What a download could not do, collected while it runs. The one way a
//! download writes a `problems` row that is not pinned to a stored record
//! or a file's stamp.
//!
//! A row clears only when the run that would clear it tried the thing
//! again. So each kind of report says what the run covered, and the
//! collector sweeps exactly that: see `docs/dev/data_architecture_ingestion.md`
//! §"Error handling" for the rule and what each kind covers.
//!
//! Adding a row needs no coverage, so a streaming download's rows ride
//! each seal ([`collecting_sealed`]) and a consumer reading a checkpoint
//! sees them. Only the sweep of listing and phase rows waits for the end,
//! because only then is it known that every one of them ran.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use anyhow::Result;

use crate::download_problems::{
    self as rows, DownloadProblem, RecordProblem, RunProblem, RunProblemKind, SilentEntry,
    SkippedRecord, Sweep, Untried,
};
use crate::download_run::OpenRun;
use crate::raw_store::Sealer;
use crate::stop::StopFlag;

/// Run a download's `body` with a collector, and write what it collected
/// when the body returns, whichever way it returns. Writing never fails
/// the run: a store that cannot take the rows is said and passed over.
/// The row the body opened in `sync_runs` ([`DownloadRun::start`])
/// closes after that, so its deltas count those rows too.
///
/// [`DownloadRun::start`]: crate::download_run::DownloadRun::start
pub async fn collecting<T, Fut>(
    pool: &sqlx::SqlitePool,
    stop: &StopFlag,
    body: impl FnOnce(RunProblems) -> Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    collecting_sealed(pool, stop, None, body).await
}

/// [`collecting`] for a download that seals as it goes: each seal first
/// writes what the run has found so far, so a checkpoint carries the
/// problems of the rows it publishes.
pub async fn collecting_sealed<T, Fut>(
    pool: &sqlx::SqlitePool,
    stop: &StopFlag,
    sealer: Option<&Sealer>,
    body: impl FnOnce(RunProblems) -> Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let problems = RunProblems {
        state: Arc::default(),
        stop: stop.clone(),
    };
    if let Some(sealer) = sealer {
        sealer.carry(Some(problems.clone()));
    }
    let result = body(problems.clone()).await;
    if let Some(sealer) = sealer {
        sealer.carry(None);
    }
    if result.is_ok() {
        problems.note_unfinished();
    }
    let ran = if result.is_ok() {
        Ran::ToItsEnd
    } else {
        Ran::PartWay
    };
    problems.write_or_say(pool, ran).await;
    if let Some(run) = problems.take_run() {
        run.close(pool, problems.ended_early(), &result).await;
    }
    result
}

/// What the downloads in one step did not finish, for the step to end on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunEnd {
    /// What the first download that stopped asking said.
    pub gave_up: Option<String>,
    /// The listings and phases that failed while the rest went on, each
    /// with its failure.
    pub unfinished: Option<String>,
}

tokio::task_local! {
    static RUN_END: Arc<Mutex<RunEnd>>;
}

/// Run `fut`, and say what the downloads inside it did not finish. It is
/// a scope rather than an error because the store is committed only on
/// `Ok`: what an unfinished run did get must still be published.
pub async fn noting_run_end<F: Future>(fut: F) -> (F::Output, RunEnd) {
    let note = Arc::new(Mutex::new(RunEnd::default()));
    let out = RUN_END.scope(note.clone(), fut).await;
    let end = std::mem::take(&mut *note.lock().unwrap());
    (out, end)
}

/// The error a step ends with when a download gave up with work left.
#[derive(Debug)]
pub struct GaveUp(pub String);

impl std::fmt::Display for GaveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the download gave up before it was done: {}", self.0)
    }
}

impl std::error::Error for GaveUp {}

/// The error a step ends with when a listing or a phase of its download
/// failed and the rest went on.
#[derive(Debug)]
pub struct Unfinished(pub String);

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "part of the download failed: {}", self.0)
    }
}

impl std::error::Error for Unfinished {}

/// Whether the body has returned `Ok`: the one thing a write in the
/// middle of a run cannot know, and what the sweep of listing and phase
/// rows waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ran {
    PartWay,
    ToItsEnd,
}

/// A cheap, cloneable handle on one run's problems; every clone adds to
/// the same set.
#[derive(Clone, Default)]
pub struct RunProblems {
    state: Arc<Mutex<State>>,
    stop: StopFlag,
}

impl std::fmt::Debug for RunProblems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunProblems").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    /// Something was reported since the last write.
    unwritten: bool,
    config: Option<Vec<DownloadProblem>>,
    run: Vec<RunProblem>,
    cut_short: bool,
    records: BTreeMap<String, Records>,
    skipped: BTreeMap<String, Vec<SkippedRecord>>,
    silent: Option<Vec<SilentEntry>>,
    /// The run's row in `sync_runs`, which closes after this set is
    /// written.
    open_run: Option<OpenRun>,
}

#[derive(Default)]
struct Records {
    failed: Vec<RecordProblem>,
    tried: Tried,
}

#[derive(Default)]
enum Tried {
    /// Only the records that failed: nothing else is cleared.
    #[default]
    OnlyTheFailed,
    /// Every record of the table, if the run was not stopped.
    All,
    /// Every record of the table but the ones this says were not tried.
    AllBut(Untried),
}

impl RunProblems {
    /// A collector nothing will write: for a test of one piece of a
    /// download, or a caller with no store. A download gets its own from
    /// [`collecting`].
    pub fn unwritten() -> Self {
        Self::default()
    }

    /// A listing that did not come back as an enumeration, so what is
    /// stored under it was left alone.
    pub fn listing(&self, name: &str, detail: impl Into<String>) {
        self.push(RunProblem::listing(name, detail));
    }

    /// A listing that was answered and still is not the whole: cut off
    /// however it is asked, or naming something it does not identify.
    /// Asking again gets the same, so it does not keep a run from being
    /// done.
    pub fn listing_cut_off(&self, name: &str, detail: impl Into<String>) {
        self.push(RunProblem::listing_cut_off(name, detail));
    }

    /// A whole phase that failed before it did its work.
    pub fn phase(&self, name: &str, detail: impl Into<String>) {
        self.push(RunProblem::phase(name, detail));
    }

    pub fn push(&self, problem: RunProblem) {
        self.extend([problem]);
    }

    /// A failure reported after a stop is the stop's, since every request
    /// after one fails at once, so it is not recorded.
    pub fn extend(&self, problems: impl IntoIterator<Item = RunProblem>) {
        if self.stop.requested() {
            return;
        }
        self.changed().run.extend(problems);
    }

    /// Run one phase so that its failure, an error or a panic, costs only
    /// that phase: `None`, and a `phase:<name>` row.
    pub async fn run_phase<T>(
        &self,
        name: &str,
        run: impl Future<Output = Result<T>>,
    ) -> Option<T> {
        let detail = match catch_unwind(run).await {
            Ok(Ok(v)) => return Some(v),
            Ok(Err(e)) => format!("{e:#}"),
            Err(panic) => format!("panicked: {}", panic_message(&*panic)),
        };
        self.phase(name, detail);
        None
    }

    /// The run ended before it tried every listing and phase: a give-up,
    /// a budget spent. Its listing and phase rows are added to the last
    /// run's, where a complete run's replace them.
    pub fn cut_short(&self) {
        self.changed().cut_short = true;
    }

    /// The run gave up with work still owed. It is cut short, and the
    /// step running it ends `incomplete` once the store is committed
    /// ([`noting_run_end`]). A run that covers less on purpose, such as
    /// one page, is only [`Self::cut_short`].
    pub fn gave_up(&self, said: impl Into<String>) {
        self.cut_short();
        let said = said.into();
        let _ = RUN_END.try_with(|end| {
            end.lock().unwrap().gave_up.get_or_insert(said);
        });
    }

    /// Tell the step which listings and phases failed, so a provider that
    /// records one and goes on need not also remember to say so. An error
    /// is a request that failed; what was answered, refused or left out on
    /// purpose is a warning or less, and asking again would not change it.
    fn note_unfinished(&self) {
        if self.stop.requested() {
            return;
        }
        let failed = unfinished(&self.lock().run);
        let Some(failed) = failed else { return };
        let _ = RUN_END.try_with(|end| {
            let mut end = end.lock().unwrap();
            end.unfinished = Some(match end.unfinished.take() {
                Some(before) => format!("{before}; {failed}"),
                None => failed,
            });
        });
    }

    /// Whether the run ended before it reached everything: told to stop,
    /// given up, or cut short. `sync_runs.status` says so.
    pub fn ended_early(&self) -> bool {
        self.stop.requested() || self.lock().cut_short
    }

    /// The configured entries upstream does not have, once every one has
    /// been looked up: the whole set, an empty one included, so an entry
    /// the config no longer names or upstream now has loses its row.
    pub fn config(&self, problems: impl IntoIterator<Item = DownloadProblem>) {
        self.changed()
            .config
            .get_or_insert_with(Vec::new)
            .extend(problems);
    }

    /// One record of `table` this run tried to fetch and could not.
    pub fn record_failed(&self, table: &str, id: &str, detail: impl Into<String>) {
        self.records_failed([RecordProblem::new(table, id, detail)]);
    }

    pub fn records_failed(&self, problems: impl IntoIterator<Item = RecordProblem>) {
        let mut state = self.changed();
        for p in problems {
            state
                .records
                .entry(p.table.clone())
                .or_default()
                .failed
                .push(p);
        }
    }

    /// This run tried every record of `table`, so a row of an earlier
    /// run that is not among this run's failures is one that fetched.
    /// A stopped run did not try them all, and clears none.
    pub fn records_tried_all(&self, table: &str) {
        self.changed()
            .records
            .entry(table.to_string())
            .or_default()
            .tried = Tried::All;
    }

    /// As [`Self::records_tried_all`], but for the records `untried`
    /// names by id: the run has no verdict on those, and their rows
    /// stand. The run says exactly what it did not try, so this holds
    /// for a stopped run too.
    pub fn records_tried_all_but(
        &self,
        table: &str,
        untried: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) {
        self.changed()
            .records
            .entry(table.to_string())
            .or_default()
            .tried = Tried::AllBut(Arc::new(untried));
    }

    /// What one `part` of the download (a feed, a file) read and chose
    /// not to store, the last time it read its input. Replaces that
    /// part's rows, so a part that read nothing this run says nothing.
    pub fn skipped(&self, part: &str, skipped: impl IntoIterator<Item = SkippedRecord>) {
        self.changed()
            .skipped
            .entry(part.to_string())
            .or_default()
            .extend(skipped);
    }

    /// The configured entries that have gone quiet, once every one has
    /// been checked: the whole set.
    pub fn silent(&self, silent: impl IntoIterator<Item = SilentEntry>) {
        self.changed()
            .silent
            .get_or_insert_with(Vec::new)
            .extend(silent);
    }

    /// How many listing or phase problems the run has so far, for a
    /// summary line.
    pub fn count(&self, kind: RunProblemKind) -> usize {
        self.lock().run.iter().filter(|p| p.kind == kind).count()
    }

    /// The listing and phase problems so far.
    pub fn run_problems(&self) -> Vec<RunProblem> {
        self.lock().run.clone()
    }

    pub(crate) async fn write_or_say(&self, pool: &sqlx::SqlitePool, ran: Ran) {
        if let Err(e) = self.write(pool, ran).await {
            tracing::warn!(
                error = %format!("{e:#}"),
                "could not record what the run could not do; the Manage row will not show it"
            );
        }
    }

    /// Write the set as it stands. Every write is the whole set so far,
    /// so one in the middle of a run and the one at its end agree; only
    /// the last may also clear the listing and phase rows the run did
    /// not report, and only if the run reached all of them.
    async fn write(&self, pool: &sqlx::SqlitePool, ran: Ran) -> Result<()> {
        fn whole(prefix: String) -> Sweep {
            Sweep { prefix, keep: None }
        }
        let stopped = self.stop.requested();
        let (sweeps, out) = {
            let mut state = self.lock();
            if !state.unwritten && ran == Ran::PartWay {
                return Ok(());
            }
            state.unwritten = false;
            let mut sweeps: Vec<Sweep> = Vec::new();
            let mut out: Vec<rows::Row> = Vec::new();
            if let Some(config) = &state.config {
                // A stopped run may not have looked every entry up.
                if !stopped {
                    sweeps.push(whole(rows::CONFIG_SWEEP.to_string()));
                }
                out.extend(rows::config_rows(config));
            }
            if ran == Ran::ToItsEnd && !stopped && !state.cut_short {
                sweeps.extend(rows::run_prefixes().into_iter().map(whole));
            }
            out.extend(rows::run_rows(&state.run));
            for (table, records) in &state.records {
                let prefix = rows::record_prefix(table);
                match &records.tried {
                    Tried::OnlyTheFailed => {}
                    Tried::All if stopped => {}
                    Tried::All => sweeps.push(whole(prefix)),
                    Tried::AllBut(untried) => sweeps.push(Sweep {
                        prefix,
                        keep: Some(untried.clone()),
                    }),
                }
                out.extend(rows::record_rows(&records.failed));
            }
            for (part, skipped) in &state.skipped {
                sweeps.push(whole(rows::skipped_prefix(part)));
                out.extend(rows::skipped_rows(part, skipped));
            }
            if let Some(silent) = &state.silent {
                sweeps.push(whole(rows::SILENT_SWEEP.to_string()));
                out.extend(rows::silent_rows(silent));
            }
            (sweeps, out)
        };
        if sweeps.is_empty() && out.is_empty() {
            return Ok(());
        }
        rows::apply(pool, &sweeps, out).await
    }

    pub(crate) fn open_run(&self, run: OpenRun) -> Result<()> {
        let mut state = self.lock();
        if state.open_run.is_some() {
            anyhow::bail!("this download already has a run open in `sync_runs`");
        }
        state.open_run = Some(run);
        Ok(())
    }

    pub(crate) fn summarize_run(&self, summary: serde_json::Value) {
        if let Some(run) = self.lock().open_run.as_mut() {
            run.summary = summary;
        }
    }

    fn take_run(&self) -> Option<OpenRun> {
        self.lock().open_run.take()
    }

    fn changed(&self) -> MutexGuard<'_, State> {
        let mut state = self.lock();
        state.unwritten = true;
        state
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one still
        // holds a whole set.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The listings and phases among `run` that failed, as one line.
fn unfinished(run: &[RunProblem]) -> Option<String> {
    let mut seen = BTreeSet::new();
    let failed: Vec<String> = run
        .iter()
        .filter(|p| p.severity == datalib_problems::Severity::Error && seen.insert(p.key()))
        .map(|p| format!("{}: {}", p.name, p.detail))
        .collect();
    (!failed.is_empty()).then(|| failed.join("; "))
}

/// Run one record's work so that a panic in it, a third-party parser's
/// on a malformed file, is that record's error and not the step's. The
/// work must not be holding anything shared that a panic would leave
/// half-changed.
pub fn catching_panics<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|panic| Err(anyhow::anyhow!("panicked: {}", panic_message(&*panic))))
}

/// Unwind safety: a piece that panics mid-write drops its transaction, and
/// a dropped sqlx transaction rolls back, so the store is left as the last
/// piece that finished left it.
async fn catch_unwind<F: Future>(run: F) -> std::thread::Result<F::Output> {
    let mut run = std::pin::pin!(run);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::{Problem, Reason, Severity};

    use datalib_sql::test_sql;
    async fn store(dir: &tempfile::TempDir) -> sqlx::SqlitePool {
        crate::doltlite_raw::open(&dir.path().join("p.doltlite_db"), &[])
            .await
            .unwrap()
    }

    async fn keys(pool: &sqlx::SqlitePool) -> Vec<String> {
        test_sql!("SELECT scope_key FROM problems ORDER BY scope_key")
            .query_scalar()
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn run(pool: &sqlx::SqlitePool, body: impl FnOnce(&RunProblems)) {
        run_with(pool, &StopFlag::new(), body).await
    }

    async fn run_with(pool: &sqlx::SqlitePool, stop: &StopFlag, body: impl FnOnce(&RunProblems)) {
        collecting(pool, stop, |problems| async move {
            body(&problems);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A complete run's listing and phase rows replace the last run's,
    /// both kinds at once, and leave the other kinds alone. A key seen
    /// again keeps its `first_seen_at_utc`; two problems on one key are
    /// one row.
    #[tokio::test]
    async fn a_complete_runs_listings_and_phases_replace_the_last_runs() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        let rows = || async {
            test_sql!(
                "SELECT scope_key, severity, reason, first_seen_at_utc FROM problems \
                 ORDER BY scope_key"
            )
            .query_as::<(String, String, String, String)>()
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        run(&pool, |p| {
            p.config([DownloadProblem::not_found(
                "only_labels",
                "x",
                "no such label",
            )]);
            p.listing("workouts", "HTTP 500");
            p.phase("weight", "cursor");
            p.listing("workouts", "a second failure on the same key");
        })
        .await;
        let first = rows().await;
        assert_eq!(
            first
                .iter()
                .map(|r| (r.0.as_str(), r.1.as_str(), r.2.as_str()))
                .collect::<Vec<_>>(),
            [
                ("config:only_labels:x", "warning", "not_found"),
                ("listing:workouts", "error", "fetch_failed"),
                ("phase:weight", "error", "fetch_failed"),
            ]
        );

        run(&pool, |p| p.listing("workouts", "HTTP 502")).await;
        let second = rows().await;
        assert_eq!(
            second.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["config:only_labels:x", "listing:workouts"],
            "the phase row went; the config row is not this run's to touch"
        );
        assert_eq!(
            second[1].3, first[1].3,
            "a key seen again keeps when it was first seen"
        );

        run(&pool, |_| {}).await;
        assert_eq!(keys(&pool).await, ["config:only_labels:x"]);
        pool.close().await;
    }

    /// A stopped run did not reach every listing, so the ones it missed
    /// would read as recovered: it clears none. What failed before the
    /// stop did fail; what fails after it is the stop's, and is not kept.
    #[tokio::test]
    async fn a_stopped_run_clears_nothing_and_keeps_only_what_failed_before_the_stop() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| p.listing("workouts", "HTTP 500")).await;

        let stop = StopFlag::new();
        run_with(&pool, &stop, |p| {
            p.listing("devices", "HTTP 502");
            stop.request();
            p.phase("weight", "interrupted");
        })
        .await;
        assert_eq!(keys(&pool).await, ["listing:devices", "listing:workouts"]);
        pool.close().await;
    }

    /// A run that gave up, or ended on an error, found what it found and
    /// did not reach the rest: its rows are added and none is cleared.
    #[tokio::test]
    async fn a_run_cut_short_adds_its_rows_and_clears_none() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| p.listing("workouts", "HTTP 500")).await;

        run(&pool, |p| {
            p.phase("fetch", "the retry loop gave up");
            p.cut_short();
        })
        .await;
        assert_eq!(keys(&pool).await, ["listing:workouts", "phase:fetch"]);

        let failed: Result<()> = collecting(&pool, &StopFlag::new(), |p| async move {
            p.listing("devices", "HTTP 502");
            anyhow::bail!("the credential was refused")
        })
        .await;
        assert!(failed.is_err());
        assert_eq!(
            keys(&pool).await,
            ["listing:devices", "listing:workouts", "phase:fetch"]
        );
        pool.close().await;
    }

    /// A run's configured-entry rows are the whole truth once it has
    /// looked every entry up: one the config no longer names, or that
    /// upstream now has, is gone. A run that never looked says nothing.
    #[tokio::test]
    async fn config_rows_are_replaced_by_a_run_that_resolved_its_config() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| {
            p.config([
                DownloadProblem::not_found("only_labels", "Recieved", "no such label"),
                DownloadProblem::forbidden("channels", "C9", "private"),
                DownloadProblem::not_found("only_labels", "Recieved", "named twice"),
            ])
        })
        .await;
        assert_eq!(
            keys(&pool).await,
            ["config:channels:C9", "config:only_labels:Recieved"],
            "an entry the config names twice is one row, and costs no other row"
        );

        run(&pool, |_| {}).await;
        assert_eq!(keys(&pool).await.len(), 2, "a run that did not look");

        run(&pool, |p| p.config([])).await;
        assert!(keys(&pool).await.is_empty());
        pool.close().await;
    }

    /// A record's row clears when the run tried it again and it is not
    /// among the failures, and only then: another table's rows, and the
    /// ones the run says it did not try, stand.
    #[tokio::test]
    async fn a_records_row_stands_until_a_run_tries_it_again() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| {
            p.record_failed("media_files", "bridge/a.mp4", "unreadable");
            p.record_failed("media_files", "holodeck/b.mp4", "unreadable");
            p.record_failed("contacts", "/dav/worf.vcf", "no UID");
        })
        .await;
        assert_eq!(keys(&pool).await.len(), 3);

        // The next run could not list `holodeck/`, and read `bridge/a.mp4`.
        run(&pool, |p| {
            p.records_tried_all_but("media_files", |id| id.starts_with("holodeck/"))
        })
        .await;
        assert_eq!(
            keys(&pool).await,
            [
                "record:contacts:/dav/worf.vcf",
                "record:media_files:holodeck/b.mp4"
            ]
        );

        // A stopped run did not try them all.
        let stop = StopFlag::new();
        stop.request();
        run_with(&pool, &stop, |p| p.records_tried_all("media_files")).await;
        assert_eq!(keys(&pool).await.len(), 2);

        run(&pool, |p| p.records_tried_all("media_files")).await;
        assert_eq!(keys(&pool).await, ["record:contacts:/dav/worf.vcf"]);
        pool.close().await;
    }

    /// A failure with no claim about the rest of its table is added, and
    /// replaces its own earlier row without a second one.
    #[tokio::test]
    async fn a_failed_record_alone_clears_nothing_else() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| {
            p.record_failed("gmail_messages", "1a0b", "HTTP 403")
        })
        .await;
        run(&pool, |p| {
            p.record_failed("gmail_messages", "1a0c", "HTTP 500");
            p.record_failed("gmail_messages", "1a0b", "HTTP 404");
        })
        .await;
        let rows: Vec<(String, String, String)> =
            test_sql!("SELECT scope_key, severity, sample FROM problems ORDER BY scope_key")
                .query_as()
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "record:gmail_messages:1a0b".to_string(),
                    Severity::Error.as_str().to_string(),
                    "HTTP 404".to_string()
                ),
                (
                    "record:gmail_messages:1a0c".to_string(),
                    Severity::Error.as_str().to_string(),
                    "HTTP 500".to_string()
                ),
            ]
        );
        pool.close().await;
    }

    /// Takeout's 321 keyless saved places once reached only the log. A
    /// part's report replaces that part's rows and no other's, since a
    /// feed that skipped an unchanged file reports nothing and its rows
    /// must stand.
    #[tokio::test]
    async fn skipped_entries_are_rows_replaced_per_part() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        let keyless = |entry: &str| SkippedRecord {
            entry: entry.to_string(),
            problem: Problem::record(Reason::NoIdentity, entry),
        };
        let long = format!("https://maps.example/?q={}", "x".repeat(500));
        run(&pool, |p| {
            p.skipped(
                "places",
                [keyless(&long), keyless("Ten Forward"), keyless(&long)],
            );
            p.skipped("watch", [keyless("https://www.youtube.com/post/Ugk")]);
        })
        .await;
        let first = keys(&pool).await;
        assert_eq!(first.len(), 3, "a repeated entry is one row: {first:?}");
        assert!(first.iter().all(|k| k.len() <= 96), "{first:?}");

        run(&pool, |p| p.skipped("places", [keyless("Ten Forward")])).await;
        let second = keys(&pool).await;
        assert_eq!(second.len(), 2, "{second:?}");
        assert!(second.iter().any(|k| k.starts_with("skipped:watch:")));

        run(&pool, |p| p.skipped("places", [])).await;
        let third = keys(&pool).await;
        assert_eq!(third.len(), 1, "{third:?}");
        assert!(third[0].starts_with("skipped:watch:"));
        pool.close().await;
    }

    /// A device that went quiet is one warning, and it goes once the
    /// device speaks again.
    #[tokio::test]
    async fn a_silent_entry_is_one_warning_until_it_speaks() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        run(&pool, |p| {
            p.silent([SilentEntry {
                name: "cargo_bay_freezer".into(),
                detail: "no readings since 2369-03-01T00:00:00+00:00".into(),
            }])
        })
        .await;
        let rows: Vec<(String, String, String)> =
            test_sql!("SELECT scope_key, severity, reason FROM problems")
                .query_as()
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            [(
                "silent:cargo_bay_freezer".to_string(),
                Severity::Warning.as_str().to_string(),
                "silent".to_string()
            )]
        );
        run(&pool, |p| p.silent([])).await;
        assert!(keys(&pool).await.is_empty());
        pool.close().await;
    }

    /// A parser that panicked on one malformed file once ended the
    /// whole step, on every run.
    #[test]
    fn a_record_that_panics_is_that_records_error() {
        let got: Result<()> = catching_panics(|| panic!("byte index 8 is not a char boundary"));
        assert_eq!(
            got.unwrap_err().to_string(),
            "panicked: byte index 8 is not a char boundary"
        );
        let owned = format!("index {} out of range", 9);
        let got: Result<()> = catching_panics(|| panic!("{owned}"));
        assert_eq!(
            got.unwrap_err().to_string(),
            "panicked: index 9 out of range"
        );
        assert_eq!(catching_panics(|| Ok(2)).unwrap(), 2);
        assert!(catching_panics(|| Err::<(), _>(anyhow::anyhow!("not a PDF"))).is_err());
    }

    /// A Google calendar whose one listing got no answer once ended
    /// `succeeded`: the row was written and the step driver never heard.
    #[tokio::test]
    async fn a_listing_or_phase_that_failed_is_told_to_the_step() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        let ((), ended) = noting_run_end(run(&pool, |p| {
            p.listing("calendar Bridge duty", "curl exit 16");
            p.phase("weight", "HTTP 500");
        }))
        .await;
        assert_eq!(ended.gave_up, None, "the source went on asking");
        assert_eq!(
            ended.unfinished.as_deref(),
            Some("calendar Bridge duty: curl exit 16; weight: HTTP 500")
        );
        pool.close().await;
    }

    /// Only what a later run could mend keeps a download from being done:
    /// every request behind these was answered, or was never made.
    #[tokio::test]
    async fn what_was_answered_or_left_out_does_not_make_a_run_unfinished() {
        let d = tempfile::tempdir().unwrap();
        let pool = store(&d).await;
        let ((), ended) = noting_run_end(run(&pool, |p| {
            p.push(RunProblem::forbidden("org Section 31", "HTTP 403"));
            p.push(RunProblem::not_asked("counts", "unknown_method"));
            p.push(RunProblem::listing_cut_off("search", "1000 results"));
            p.record_failed("messages", "1a0b", "HTTP 404");
            p.config([DownloadProblem::not_found("calendars", "x", "no such")]);
        }))
        .await;
        assert_eq!(ended.unfinished, None);

        let stop = StopFlag::new();
        let ((), ended) = noting_run_end(run_with(&pool, &stop, |p| {
            p.listing("devices", "HTTP 502");
            stop.request();
        }))
        .await;
        assert_eq!(ended.unfinished, None, "a stopped run says it was stopped");
        pool.close().await;
    }

    /// A panic inside one Takeout feed once ended the whole step.
    #[tokio::test]
    async fn a_phase_that_panics_costs_only_itself() {
        let problems = RunProblems::unwritten();
        let got: Option<()> = problems
            .run_phase("youtube_watch_history", async {
                panic!("sliced through a character")
            })
            .await;
        assert!(got.is_none());
        assert_eq!(
            problems.run_problems(),
            [RunProblem::phase(
                "youtube_watch_history",
                "panicked: sliced through a character"
            )]
        );
        let failing =
            async { Err::<(), _>(anyhow::anyhow!("not JSON").context("parse Saved Places.json")) };
        assert!(problems
            .run_phase("maps_saved_places", failing)
            .await
            .is_none());
        assert_eq!(
            problems.run_problems()[1].detail,
            "parse Saved Places.json: not JSON"
        );
        assert_eq!(
            problems.run_phase("maps_reviews", async { Ok(2) }).await,
            Some(2)
        );
        assert_eq!(problems.count(RunProblemKind::Phase), 2);
    }
}
