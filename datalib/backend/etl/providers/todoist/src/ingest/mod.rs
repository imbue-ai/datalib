//! Todoist downloader: mirror an account's projects, sections, tasks,
//! comments and labels, as Todoist's API v1 sends them, into one raw
//! table per kind.
//!
//! A run is two parts. `/sync` with the token the last answer carried
//! sends everything that changed since (everything, with `*`, the first
//! time); what it sends and the new token are stored in one transaction,
//! so the token never claims what the store does not hold. Then the
//! tasks completed since the last run are read from
//! `/tasks/completed/by_completion_date`, which keeps them apart from
//! the open ones, newest first, a window of completion time at a time,
//! each window's span written to `coverage` with its last page.
//! INGEST.md has the whole account.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod windows;

use anyhow::Result;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::raw_store::{Batch, Sealer};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_todoist_config::TodoistSync;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};

pub use api::TodoistError;
pub use db::{db_path_for, RawDb};

use db::Stamp;
use schema_raw::{Kind, COMPLETED_ITEMS, COMPLETED_SCOPE, SYNCED, USER};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub sync: TodoistSync,
    /// The top of the walk of completed tasks; the clock when `None`.
    /// A test pins it, so its tape knows which windows are asked for.
    pub now: Option<IsoOffsetTimestamp>,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
    pub sealer: Option<Sealer>,
}

impl FetchOptions {
    pub fn new(db: RawDb) -> Self {
        FetchOptions {
            db,
            latchkey: LatchkeySettings::default(),
            sync: TodoistSync::default(),
            now: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    /// `/sync` answers read: one, or two after a full sync.
    pub syncs: usize,
    /// Whether one of them was a full sync.
    pub full_sync: bool,
    /// Records the syncs sent and the store took.
    pub synced: usize,
    /// Windows of completion time read whole.
    pub windows: usize,
    /// Completed tasks listed in them.
    pub completed_listed: usize,
    /// Of those, the ones written: new, or changed since they were held.
    pub completed_written: usize,
    pub requests: u64,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "syncs={} full_sync={} synced={} windows={} completed_listed={} \
             completed_written={} requests={}",
            self.syncs,
            self.full_sync,
            self.synced,
            self.windows,
            self.completed_listed,
            self.completed_written,
            self.requests
        )
    }
}

/// The run ends here: a refused credential or a retry guard that gave
/// up would fail every request left.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RunEnded(pub TodoistError);

struct Ctx<'a> {
    opts: &'a FetchOptions,
    found: &'a RunProblems,
    now: String,
    requests: u64,
    /// Every read and write of the store goes through this until the run
    /// ends: it holds the store's one connection between seals.
    writes: Batch<'a>,
}

impl Ctx<'_> {
    fn stopped(&self) -> bool {
        self.opts.control.stop.requested()
    }

    async fn ask(
        &mut self,
        req: datalib_etl_web::http::HttpRequest,
    ) -> Result<Value, TodoistError> {
        self.requests += 1;
        api::send(req, &self.opts.latchkey).await
    }

    async fn wrote(&mut self, rows: usize) -> Result<()> {
        self.opts.progress.inc(rows as u64);
        self.writes.wrote(rows as u64).await
    }

    /// `/sync` from the stored token, and once more after a full sync:
    /// a large account's full sync can be generated a while before it is
    /// sent (`full_sync_date_utc`), and Todoist's advice is to follow it
    /// with an incremental one.
    async fn sync(&mut self, s: &mut FetchSummary) -> Result<()> {
        self.opts.progress.set_message("sync");
        for _ in 0..2 {
            if self.stopped() {
                return Ok(());
            }
            let token = db::sync_token(self.writes.tx().await?)
                .await?
                .unwrap_or_else(|| "*".to_string());
            let answer = match self.ask(api::sync_request(&token)).await {
                Ok(answer) => answer,
                Err(_) if self.stopped() => return Ok(()),
                Err(e) if e.ends_the_run() => return Err(RunEnded(e).into()),
                Err(e) => anyhow::bail!("Todoist's /sync failed, so nothing was updated: {e}"),
            };
            let full = answer.get("full_sync").and_then(Value::as_bool) == Some(true);
            self.store_sync(&answer, s).await?;
            s.syncs += 1;
            s.full_sync |= full;
            if !full {
                break;
            }
        }
        Ok(())
    }

    /// One `/sync` answer, and the token it carries, in one transaction.
    async fn store_sync(&mut self, answer: &Value, s: &mut FetchSummary) -> Result<()> {
        let Some(token) = answer.get("sync_token").and_then(Value::as_str) else {
            anyhow::bail!("Todoist's /sync answered with no sync_token, so nothing was stored");
        };
        let now = IsoOffsetTimestamp::now_local();
        let tx = self.writes.tx().await?;
        let mut n = 0;
        for kind in SYNCED {
            let records = records_of(*kind, answer);
            let stamp = if kind.table == USER.table {
                Stamp::FirstSeen
            } else {
                Stamp::Fetched
            };
            n += db::write(tx, *kind, &records, stamp, &now).await?;
        }
        let generated = answer.get("full_sync_date_utc").and_then(Value::as_str);
        db::set_sync_token(tx, token, generated).await?;
        s.synced += n;
        self.wrote(n.max(1)).await
    }

    /// Where the walk of completed tasks stops: the configured date, else
    /// the day the account joined.
    async fn floor(&mut self) -> Result<Option<String>> {
        if let Some(since) = &self.opts.sync.completed_since {
            return Ok(windows::normalize(since));
        }
        let joined = db::joined_at(self.writes.tx().await?).await?;
        Ok(joined.as_deref().and_then(windows::normalize))
    }

    async fn completed(&mut self, s: &mut FetchSummary) -> Result<()> {
        if !self.opts.sync.completed || self.stopped() {
            return Ok(());
        }
        self.opts.progress.set_message(COMPLETED_ITEMS.table);
        let Some(floor) = self.floor().await? else {
            self.found.phase(
                COMPLETED_ITEMS.table,
                "the account's user record says nothing of when it joined Todoist, so it is \
                 not known how far back to read completed tasks; set `api.completed_since` \
                 to read them",
            );
            return Ok(());
        };
        let held = coverage::held(&mut **self.writes.tx().await?, COMPLETED_SCOPE).await?;
        for window in windows::to_read(&floor, &self.now, &held) {
            if !self.window(&window, s).await? {
                return Ok(());
            }
            s.windows += 1;
        }
        Ok(())
    }

    /// Every page of one window, the window's span covered with its last
    /// page. `false` when the walk should go no further.
    async fn window(&mut self, window: &Span, s: &mut FetchSummary) -> Result<bool> {
        let mut cursor: Option<String> = None;
        loop {
            if self.stopped() {
                return Ok(false);
            }
            let req = api::completed_request(&window.lo, &window.hi, cursor.as_deref());
            let page = match self.ask(req).await {
                Ok(page) => page,
                Err(_) if self.stopped() => return Ok(false),
                Err(e) if e.ends_the_run() => return Err(RunEnded(e).into()),
                Err(e) => {
                    self.found.listing(
                        COMPLETED_ITEMS.table,
                        format!(
                            "the read of tasks completed between {} and {} stopped part-way, \
                             so older completed tasks were not looked at; the next run \
                             carries on from there: {e}",
                            window.lo, window.hi
                        ),
                    );
                    return Ok(false);
                }
            };
            let items = page
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("a page of completed tasks has no `items` list"))?;
            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(String::from);
            let last = cursor.is_none();
            self.store_completed(items, last.then_some(window), s)
                .await?;
            if last {
                return Ok(true);
            }
        }
    }

    /// The tasks of one page not already held at their version, with
    /// `cover` claimed.
    async fn store_completed(
        &mut self,
        items: Vec<Value>,
        cover: Option<&Span>,
        s: &mut FetchSummary,
    ) -> Result<()> {
        s.completed_listed += items.len();
        let table = COMPLETED_ITEMS.table;
        let ids: Vec<String> = items.iter().filter_map(db::id_of).collect();
        let held = owed::held_versions_in(
            self.writes.tx().await?,
            table,
            ids.iter().map(String::as_str),
        )
        .await?;
        let changed: Vec<Value> = items
            .into_iter()
            .filter(|r| {
                !db::id_of(r)
                    .and_then(|id| held.get(id.as_str()))
                    .is_some_and(|h| h.satisfies(&db::version_of(r)))
            })
            .collect();
        let now = IsoOffsetTimestamp::now_local();
        let tx = self.writes.tx().await?;
        let n = db::write(tx, COMPLETED_ITEMS, &changed, Stamp::Fetched, &now).await?;
        for r in &changed {
            if let Some(id) = db::id_of(r) {
                owed::set_held_version(tx, table, &id, db::version_of(r).as_deref()).await?;
            }
        }
        if let Some(span) = cover {
            coverage::cover(tx, COMPLETED_SCOPE, span.clone()).await?;
        }
        s.completed_written += n;
        self.wrote(n).await
    }
}

/// The records `/sync` sent for `kind`: a list, or for the account one
/// object.
fn records_of(kind: Kind, answer: &Value) -> Vec<Value> {
    let Some(key) = kind.sync_key else {
        return Vec::new();
    };
    match answer.get(key) {
        Some(Value::Array(records)) => records.clone(),
        Some(record @ Value::Object(_)) => vec![record.clone()],
        _ => Vec::new(),
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| download(opts, found))
        .await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();
    let run_config = json!({
        "completed": opts.sync.completed,
        "completed_since": opts.sync.completed_since,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let now = opts
        .now
        .clone()
        .unwrap_or_else(IsoOffsetTimestamp::now_local);
    let now = windows::stamp(now.to_unix_millis()).expect("now is a representable instant");
    let mut ctx = Ctx {
        opts: &opts,
        found: &found,
        now,
        requests: 0,
        writes: Batch::new(db.pool(), opts.sealer.as_ref()),
    };
    let mut s = FetchSummary::default();
    let result = phases(&mut ctx, &mut s).await;
    s.requests = ctx.requests;
    // What failed may have struck between two writes that belong
    // together, so it is rolled back; the batch frees the store's one
    // connection either way before the run's record is written.
    let Ctx { writes, .. } = ctx;
    let result = match result {
        Ok(()) => writes.finish().await,
        Err(e) => {
            drop(writes);
            Err(e)
        }
    };
    run.finish(&result, &s).await;
    result.map(|()| s)
}

async fn phases(ctx: &mut Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let ended = match run_phases(ctx, s).await {
        Ok(()) => return Ok(()),
        Err(e) => match e.downcast::<RunEnded>() {
            Ok(ended) => ended,
            Err(e) => return Err(e),
        },
    };
    if s.syncs == 0 {
        anyhow::bail!("Todoist refused the run before anything came: {}", ended.0);
    }
    let name = match &ended.0 {
        TodoistError::Unauthorized(_) => "credential",
        _ => "gave_up",
    };
    let problem = RunProblem::phase(
        name,
        format!(
            "Todoist stopped answering part-way through, so the run stopped; what it fetched \
             before is kept, and the next run carries on from there: {}",
            ended.0
        ),
    );
    ctx.found.gave_up(problem.detail.clone());
    ctx.found.push(problem);
    Ok(())
}

async fn run_phases(ctx: &mut Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    ctx.sync(s).await?;
    ctx.completed(s).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_comes_as_one_object_and_the_rest_as_lists() {
        let answer = json!({"user": {"id": "1701"}, "items": [{"id": "a"}, {"id": "b"}],
                            "labels": null});
        assert_eq!(records_of(USER, &answer).len(), 1);
        assert_eq!(records_of(schema_raw::ITEMS, &answer).len(), 2);
        assert!(records_of(schema_raw::LABELS, &answer).is_empty());
        assert!(records_of(COMPLETED_ITEMS, &answer).is_empty());
    }
}
