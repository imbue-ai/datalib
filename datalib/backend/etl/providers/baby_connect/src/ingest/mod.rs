//! Baby Connect downloader: copy every entry an account logged, one day
//! at a time, into raw tables of kids, the people who logged, and the
//! entries.
//!
//! A run reads the home page, which lists the children and carries the
//! token a day's request sends, then asks for every day from the eldest
//! child's birthday (or `api.since`) to today (or `api.until`) that the
//! store's `coverage` does not hold, oldest first. A day's entries and
//! the record that the day was read land in one transaction. The last
//! two days are never recorded as read, so every run reads them again.
//! INGEST.md has the whole account.

pub mod api;
pub mod days;
pub mod db;
pub mod home;
pub mod schema_raw;

use anyhow::Result;
use chrono::NaiveDate;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::raw_store::{Batch, Sealer};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::upstream::Upstream;
use datalib_etl_baby_connect_config::BabyConnectSync;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use datalib_time::{IsoOffsetTimestamp, TimeBound};
use serde::Serialize;
use serde_json::{json, Value};

pub use api::BabyConnectError;
pub use db::{db_path_for, RawDb};

use schema_raw::{DAYS_SCOPE, ENTRIES, KIDS, USERS};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub sync: BabyConnectSync,
    /// Which day is today; the clock when `None`. A test pins it, so its
    /// tape knows which days are asked for.
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
            sync: BabyConnectSync::default(),
            now: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub kids: usize,
    /// Days asked for, and of those, the ones recorded as read for good.
    pub days: usize,
    pub days_settled: usize,
    /// Entries the days' answers carried for their own day.
    pub entries: usize,
    pub requests: u64,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "kids={} days={} days_settled={} entries={} requests={}",
            self.kids, self.days, self.days_settled, self.entries, self.requests
        )
    }
}

/// The run ends here: a refused session or a retry guard that gave up
/// would fail every request left.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RunEnded(pub BabyConnectError);

struct Ctx<'a> {
    opts: &'a FetchOptions,
    found: &'a RunProblems,
    today: NaiveDate,
    requests: u64,
    /// Every read and write of the store goes through this until the run
    /// ends: it holds the store's one connection between seals.
    writes: Batch<'a>,
}

impl Ctx<'_> {
    fn stopped(&self) -> bool {
        self.opts.control.stop.requested()
    }

    async fn wrote(&mut self, rows: usize) -> Result<()> {
        self.writes.wrote(rows as u64).await
    }

    async fn home(&mut self, s: &mut FetchSummary) -> Result<home::Home> {
        self.opts.progress.set_message("home");
        self.requests += 1;
        let resp = match api::send(api::home_request(), &self.opts.latchkey).await {
            Ok(resp) => resp,
            Err(e) if e.ends_the_run() => return Err(RunEnded(e).into()),
            Err(e) => {
                return Err(Upstream::error(
                    e.upstream(),
                    format!("Baby Connect's home page did not load, so nothing was read: {e}"),
                ))
            }
        };
        let Some(home) = home::parse(&resp.body_str()) else {
            let e = BabyConnectError::Unauthorized(format!(
                "{}: the page names no children and carries no token, which is the page a \
                 signed-out session is shown. {}",
                api::home_request().url,
                api::sign_in()
            ));
            return Err(RunEnded(e).into());
        };
        let now = IsoOffsetTimestamp::now_local();
        let tx = self.writes.tx().await?;
        let n = db::write(tx, KIDS, &home.kids, &now).await?;
        s.kids = n;
        self.wrote(n.max(1)).await?;
        Ok(home)
    }

    /// The first and last day to read: the configured window, else the
    /// eldest child's birthday and today.
    fn window(&self, kids: &[Value]) -> Result<Option<(NaiveDate, NaiveDate)>> {
        let now = self
            .opts
            .now
            .clone()
            .unwrap_or_else(IsoOffsetTimestamp::now_local);
        let bound = |s: &Option<String>| -> Result<Option<NaiveDate>> {
            let Some(s) = s else { return Ok(None) };
            let at = TimeBound::absolute(s, &now)?;
            Ok(days::day(&at))
        };
        let from = match bound(&self.opts.sync.since)? {
            Some(d) => d,
            None => match kids.iter().filter_map(days::birthday).min() {
                Some(d) => d,
                None => return Ok(None),
            },
        };
        let to = bound(&self.opts.sync.until)?.map_or(self.today, |d| d.min(self.today));
        Ok(Some((from, to)))
    }

    async fn walk(&mut self, home: &home::Home, s: &mut FetchSummary) -> Result<()> {
        let Some((from, to)) = self.window(&home.kids)? else {
            self.found.phase(
                ENTRIES.table,
                "no child on the account has a birthday, so it is not known which day to start \
                 from; set `api.since` to read the entries",
            );
            return Ok(());
        };
        // The web page asks for one child by id when there is only one,
        // and for every child at once (`0`) otherwise.
        let kid = match home.kids.as_slice() {
            [only] => db::id_of(only).unwrap_or_else(|| "0".to_string()),
            _ => "0".to_string(),
        };
        let mut held = coverage::held(&mut **self.writes.tx().await?, DAYS_SCOPE).await?;
        let owed = days::owed(from, to, &held);
        self.opts.progress.set_message(ENTRIES.table);
        self.opts.progress.set_length(Some(owed.len() as u64));
        for d in owed {
            if self.stopped() {
                return Ok(());
            }
            self.requests += 1;
            let req = api::list_request(&kid, days::pdt(d), &home.token);
            let answer = match api::send_json(req, &self.opts.latchkey).await {
                Ok(answer) => answer,
                Err(_) if self.stopped() => return Ok(()),
                Err(e) if e.ends_the_run() => return Err(RunEnded(e).into()),
                Err(e) => {
                    self.found.listing(
                        ENTRIES.table,
                        format!(
                            "the entries of {} did not come, so that day was not read; the \
                             next run asks for it again: {e}",
                            days::key(d)
                        ),
                    );
                    self.opts.progress.inc(1);
                    continue;
                }
            };
            if let Some(span) = self.store_day(d, &answer, &held, s).await? {
                held.push(span);
                held = coverage::merged(held);
            }
            self.opts.progress.inc(1);
        }
        Ok(())
    }

    /// One day's entries and the people who logged them, and the day
    /// recorded as read when it is settled, in one transaction. Returns
    /// the span recorded.
    async fn store_day(
        &mut self,
        d: NaiveDate,
        answer: &Value,
        held: &[Span],
        s: &mut FetchSummary,
    ) -> Result<Option<Span>> {
        let entries = days::entries_of(answer, d);
        let users = days::users_of(answer);
        let now = IsoOffsetTimestamp::now_local();
        let settled = days::settled(d, self.today);
        let tx = self.writes.tx().await?;
        let mut n = db::write(tx, USERS, &users, &now).await?;
        n += db::write(tx, ENTRIES, &entries, &now).await?;
        let span = settled.then(|| days::span_for(d, held));
        if let Some(span) = &span {
            coverage::cover(tx, DAYS_SCOPE, span.clone()).await?;
            s.days_settled += 1;
        }
        s.days += 1;
        s.entries += entries.len();
        self.wrote(n.max(1)).await?;
        Ok(span)
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
        "since": opts.sync.since,
        "until": opts.sync.until,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let now = opts
        .now
        .clone()
        .unwrap_or_else(IsoOffsetTimestamp::now_local);
    let today = days::day(&now.to_rfc3339()).expect("an RFC 3339 stamp starts with its day");
    let mut ctx = Ctx {
        opts: &opts,
        found: &found,
        today,
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
    run.finish(found.ended_early(), &result, &s).await;
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
    if s.days == 0 {
        return Err(Upstream::error(
            ended.0.upstream(),
            format!(
                "Baby Connect refused the run before any day came: {}",
                ended.0
            ),
        ));
    }
    let name = match &ended.0 {
        BabyConnectError::Unauthorized(_) => "credential",
        _ => "gave_up",
    };
    let problem = RunProblem::phase(
        name,
        format!(
            "Baby Connect stopped answering part-way through, so the run stopped; the days read \
             before are kept, and the next run carries on from there: {}",
            ended.0
        ),
    );
    ctx.found.gave_up(problem.detail.clone());
    ctx.found.push(problem);
    Ok(())
}

async fn run_phases(ctx: &mut Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let home = ctx.home(s).await?;
    if ctx.stopped() {
        return Ok(());
    }
    ctx.walk(&home, s).await
}
