//! Garmin Connect → doltlite. Five walks over one account: the per-day
//! metrics, the weigh-ins, the activities (listing, detail and FIT
//! file), the optional per-day wellness FIT bundle, and the small
//! whole-account listings. Garmin has no "what changed since" API, so
//! incrementality is a trailing re-fetch window per walk, and
//! UPSERT-by-upstream-id makes the overlap free.
//!
//! No walk keeps a cursor. What a run fetches is what the window wants
//! less what the store holds (docs/dev/plans/sync_state.md): a day is
//! held by its row and the date it was fetched on, the activity listing
//! by `coverage` spans, a detail and a file by the listing version they
//! were fetched for.
//!
//! A walk prunes what its listing did not name only when the listing
//! is an enumeration: an array (every page of it, for the paged ones),
//! with no request failing. Anything else — an object with no array
//! inside, a 204/404/empty body, a page that errored — is byte-similar
//! to "everything was deleted" and must not be read that way; it is a
//! `problems` row instead, and the stored rows stay.

pub mod api;
pub mod db;
pub mod schema_raw;

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration, NaiveDate};
use serde_json::Value;
use tracing::{info, warn};

use datalib_etl::blob_cas::{blake3_hex, CasEdgeAccumulator};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::coverage::{self, Span};
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::download_problems::RunProblem;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_garmin_config::{GarminApi, DEFAULT_SINCE_DAYS};

use api::{Fetched, GarminClient, GarminError};
pub use db::{db_path_for, RawDb};
use db::{ActivityWork, Enumerated, ACTIVITIES_SCOPE};
use schema_raw::*;

/// Where the window starts when the config names no `since`: a year
/// before the first run, kept here so later runs start there too.
const DEFAULT_SINCE_SCOPE: &str = "garmin:default_since";

/// Days of one metric written per transaction.
const DAILY_BATCH_DAYS: usize = 31;
/// Files and wellness days written per transaction.
const FILE_BATCH: usize = 20;

/// A metric that fails this many days in a row is abandoned for the
/// run: a dead endpoint should not cost a request per day of history.
const CONSECUTIVE_FAILURE_BUDGET: u32 = 10;
/// `/weight-service/weight/range/<start>/<end>` per request.
pub const WEIGHT_CHUNK_DAYS: i64 = 90;
pub const ACTIVITY_PAGE: usize = 100;
/// `start`/`limit` page of the workout and goal listings.
pub const ITEM_PAGE: usize = 100;
/// Pages of a listing before the walk is declared runaway — and, since
/// it never reached a short page, not an enumeration.
const MAX_LISTING_PAGES: usize = 2000;

/// The whole-account listings, in the order they are walked. A paged
/// kind takes `start`/`limit` and is walked to its first short page;
/// the rest answer the whole list in one response.
pub const ITEM_KINDS: &[ItemKind] = &[
    ItemKind {
        name: "personal_records",
        id_keys: &["id"],
        paged: false,
    },
    ItemKind {
        name: "gear",
        id_keys: &["gearPk", "uuid"],
        paged: false,
    },
    ItemKind {
        name: "badges",
        id_keys: &["badgeId", "badgeUuid"],
        paged: false,
    },
    ItemKind {
        name: "workouts",
        id_keys: &["workoutId"],
        paged: true,
    },
    ItemKind {
        name: "goals",
        id_keys: &["id", "goalId"],
        paged: true,
    },
];

#[derive(Debug, Clone, Copy)]
pub struct ItemKind {
    pub name: &'static str,
    /// The upstream id, first present key wins.
    pub id_keys: &'static [&'static str],
    pub paged: bool,
}

/// The path of one item listing, or of one of its pages. `None` for
/// gear on an account whose profile carries no `profileId`. Also what
/// the synthesizer builds its fixtures under.
pub fn item_listing_path(
    kind: &str,
    display_name: &str,
    profile_pk: Option<&str>,
    offset: usize,
) -> Option<String> {
    Some(match kind {
        "personal_records" => format!(
            "/personalrecord-service/personalrecord/prs/{}",
            urlencoding::encode(display_name)
        ),
        "gear" => format!(
            "/gear-service/gear/filterGear?userProfilePk={}",
            profile_pk?
        ),
        "badges" => "/badge-service/badge/earned".to_string(),
        "workouts" => format!("/workout-service/workouts?start={offset}&limit={ITEM_PAGE}"),
        "goals" => format!(
            "/goal-service/goal/goals?status=active&start={offset}&limit={ITEM_PAGE}&sortOrder=asc"
        ),
        other => panic!("item_listing_path: {other:?} is not in ITEM_KINDS"),
    })
}

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub latchkey: datalib_etl::http::LatchkeySettings,
    pub api: GarminApi,
    /// The run's local calendar date: the last day every walk reaches,
    /// and the date every row this run fetches is stamped with.
    pub today: NaiveDate,
    pub progress: Progress,
    pub control: DownloadControl,
    pub sealer: Option<Sealer>,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub requests: u64,
    pub metrics: usize,
    pub days: usize,
    pub weigh_ins: usize,
    pub weigh_ins_pruned: usize,
    pub activities_listed: usize,
    pub activities_fetched: usize,
    pub activities_pruned: usize,
    pub activity_files: usize,
    pub wellness_files: usize,
    pub devices: usize,
    pub items: usize,
    pub items_pruned: usize,
    /// Listings that were not enumerations, so were not pruned to.
    pub listings_failed: usize,
    /// Phases that failed wholesale; the others ran.
    pub phases_failed: usize,
    pub errors: usize,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "requests={} metrics={} days={} weigh_ins={} weigh_ins_pruned={} \
             activities_listed={} activities_fetched={} activities_pruned={} \
             activity_files={} wellness_files={} devices={} items={} items_pruned={} \
             listings_failed={} phases_failed={} errors={}",
            self.requests,
            self.metrics,
            self.days,
            self.weigh_ins,
            self.weigh_ins_pruned,
            self.activities_listed,
            self.activities_fetched,
            self.activities_pruned,
            self.activity_files,
            self.wellness_files,
            self.devices,
            self.items,
            self.items_pruned,
            self.listings_failed,
            self.phases_failed,
            self.errors,
        )
    }
}

/// The account facts every per-user path needs, read once per run.
struct Account {
    display_name: String,
    profile_pk: Option<String>,
}

pub fn date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("date {s:?}"))
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// The first day the walks cover: the configured `since`, else the
/// default recorded by the first run, else a year before today. Taken
/// afresh each run, a year before today would move a day a day.
fn window_start(configured: Option<&str>, recorded: Option<&str>, today: NaiveDate) -> String {
    configured
        .or(recorded)
        .map(str::to_string)
        .unwrap_or_else(|| ymd(today - Duration::days(DEFAULT_SINCE_DAYS)))
}

/// Today, or the configured `until` when it is earlier: a window that
/// ends in the past stays that window, run after run.
fn walk_end(today: NaiveDate, until: Option<&str>) -> Result<NaiveDate> {
    Ok(match until {
        Some(u) => date(u)?.min(today),
        None => today,
    })
}

/// The first date on which what Garmin answers for `day` is taken as
/// final: the watch syncs late and Garmin recomputes, so a day is asked
/// for again until `refresh` days after it, and never settles while it
/// is still that day.
fn settles_on(day: NaiveDate, refresh: Duration) -> NaiveDate {
    day + refresh.max(Duration::days(1))
}

/// The days of `since..=end` to fetch, oldest first: those with no row,
/// and those whose row was fetched before the day had settled. `held` is
/// `calendar_date → fetched_on`.
fn owed_days(
    since: NaiveDate,
    end: NaiveDate,
    held: &HashMap<String, String>,
    refresh: Duration,
) -> Vec<NaiveDate> {
    since
        .iter_days()
        .take_while(|day| *day <= end)
        .filter(|day| match held.get(&ymd(*day)) {
            Some(fetched_on) => *fetched_on < ymd(settles_on(*day, refresh)),
            None => true,
        })
        .collect()
}

/// The start date the activity listing is asked from: the lowest one
/// `held` does not cover, or the start of the refresh window when that is
/// earlier. The listing takes a start and runs to the present, so one
/// walk from there covers every gap above it too.
fn listing_start(want: &Span, held: &[Span], refresh_from: NaiveDate) -> Result<NaiveDate> {
    let lowest_gap = coverage::gaps(want, held)
        .first()
        .map(|gap| date(&gap.lo))
        .transpose()?;
    Ok(lowest_gap
        .map_or(refresh_from, |gap| gap.min(refresh_from))
        .max(date(&want.lo)?))
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        walk_account(opts, found)
    })
    .await
}

async fn walk_account(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db;
    let api = opts.api;
    let recorded_since = db.marker(DEFAULT_SINCE_SCOPE).await?;
    let since_str = window_start(api.since.as_deref(), recorded_since.as_deref(), opts.today);
    if api.since.is_none() && recorded_since.is_none() {
        db.set_marker(DEFAULT_SINCE_SCOPE, &since_str).await?;
    }
    let since = date(&since_str)?;
    let end = walk_end(opts.today, api.until.as_deref())?;
    let refresh = Duration::days(api.refresh_days());
    let mut client = GarminClient::new(opts.latchkey);
    let mut s = FetchSummary::default();
    let progress = &opts.progress;

    // Every phase after the account is independent, and one that fails
    // must not cost the others — except an auth failure, which they would
    // all share. The run still returns `Ok` after a failed phase: the
    // step driver commits and reports the store's problem counts only on
    // `Ok`, so `Err` here would hide the very rows that say what failed.
    let (account, settings_failed) = fetch_account(&mut client, &db, &opts.control.stop).await?;
    if let Some(problem) = settings_failed {
        s.errors += 1;
        s.listings_failed += 1;
        found.push(problem);
    }
    db.repair_shared_file_hashes(api.activity_files(), api.wellness_files())
        .await?;
    let mut walk = Walk {
        db: &db,
        client: &mut client,
        account: &account,
        api: &api,
        since,
        end,
        today: opts.today,
        refresh,
        sealer: opts.sealer.as_ref(),
        progress,
        stop: &opts.control.stop,
        found,
    };

    // A stop makes every request after it fail at once. None of those
    // failures is Garmin's, so none becomes a `problems` row, and a phase
    // not yet started is left for the next run.
    macro_rules! phase {
        ($name:literal, $fut:expr) => {
            progress.set_message(concat!("garmin: ", $name));
            if walk.stopping() {
                info!(event = "garmin_phase_skipped", phase = $name, "told to stop; leaving this phase for the next run");
            } else if let Err(e) = $fut.await {
                if is_auth(&e) {
                    return Err(e.context(concat!("garmin ", $name)));
                }
                let detail = format!("{e:#}");
                if walk.stopping() {
                    info!(event = "garmin_phase_stopped", phase = $name, error = %detail, "a phase ended on the stop");
                } else {
                    s.errors += 1;
                    s.phases_failed += 1;
                    walk.found.phase($name, detail);
                }
            }
        };
    }
    phase!("devices", walk.devices(&mut s));
    phase!("daily", walk.daily(&mut s));
    phase!("weight", walk.weight(&mut s));
    phase!("activities", walk.activities(&mut s));
    phase!("wellness", walk.wellness(&mut s));
    phase!("items", walk.items(&mut s));
    s.requests = walk.client.requests;
    Ok(s)
}

/// Everything one run's walks share.
struct Walk<'a> {
    db: &'a RawDb,
    client: &'a mut GarminClient,
    account: &'a Account,
    api: &'a GarminApi,
    since: NaiveDate,
    /// The last day this run walks: today, or `until` when it is earlier.
    end: NaiveDate,
    today: NaiveDate,
    refresh: Duration,
    sealer: Option<&'a Sealer>,
    progress: &'a Progress,
    stop: &'a StopFlag,
    found: RunProblems,
}

/// What a listing request came back as. Only a complete one is an
/// enumeration, and only a complete one is pruned to. `rows` on an
/// incomplete one are the pages that did arrive; upserting them loses
/// nothing.
struct Listed {
    rows: Vec<Value>,
    /// Why this is not an enumeration, when it is not.
    incomplete: Option<String>,
}

const NOTHING: &str = "answered with nothing (204, 404 or an empty body)";

/// The array a listing answered with, or why it is not one. `wrapped`
/// accepts an object holding the array under some key, which the item
/// listings do; the rest answer a bare array.
fn listing_array(
    fetched: Fetched<Value>,
    wrapped: bool,
) -> std::result::Result<Vec<Value>, String> {
    match fetched {
        Fetched::Some(Value::Array(a)) => Ok(a),
        Fetched::Some(Value::Object(o)) if wrapped => {
            let keys: Vec<&String> = o.keys().collect();
            let inside = format!("answered an object with no array inside: keys {keys:?}");
            o.into_iter()
                .find_map(|(_, v)| match v {
                    Value::Array(a) => Some(a),
                    _ => None,
                })
                .ok_or(inside)
        }
        Fetched::Some(other) => {
            let preview: String = other.to_string().chars().take(120).collect();
            Err(format!("expected an array, got {preview}"))
        }
        Fetched::Nothing => Err(NOTHING.to_string()),
    }
}

/// The account, and the problem of a user-settings fetch that failed.
async fn fetch_account(
    client: &mut GarminClient,
    db: &RawDb,
    stop: &StopFlag,
) -> Result<(Account, Option<RunProblem>)> {
    let profile = match client
        .get_json("/userprofile-service/socialProfile")
        .await
        .context("garmin socialProfile")?
    {
        Fetched::Some(v) => v,
        Fetched::Nothing => bail!("garmin socialProfile answered with nothing"),
    };
    let display_name = profile["displayName"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("garmin socialProfile carries no displayName"))?
        .to_string();
    let profile_pk = profile["profileId"]
        .as_i64()
        .or_else(|| profile["id"].as_i64())
        .map(|n| n.to_string());
    let mut rows = vec![AccountRow {
        id_and_payload: WirePayload {
            id: ACCOUNT_SOCIAL_PROFILE.into(),
            payload: profile.to_string(),
        },
        display_name: Some(display_name.clone()),
    }];
    // The settings are not needed for anything else the run does, so a
    // failure costs only them: the stored row stays.
    let mut settings_failed = None;
    match client
        .get_json("/userprofile-service/userprofile/user-settings")
        .await
    {
        Ok(Fetched::Some(settings)) => rows.push(AccountRow {
            id_and_payload: WirePayload {
                id: ACCOUNT_USER_SETTINGS.into(),
                payload: settings.to_string(),
            },
            display_name: Some(display_name.clone()),
        }),
        Ok(Fetched::Nothing) => {}
        Err(e) if is_auth(&e) => return Err(e.context("garmin user-settings")),
        Err(e) if stop.requested() => {
            info!(event = "garmin_user_settings_stopped", error = %format!("{e:#}"), "user-settings ended on the stop");
        }
        Err(e) => {
            settings_failed = Some(RunProblem::listing("user_settings", format!("{e:#}")));
        }
    }
    upsert(db, &rows).await?;
    Ok((
        Account {
            display_name,
            profile_pk,
        },
        settings_failed,
    ))
}

async fn upsert<T: datalib_etl::bulk::BulkUpsertable>(db: &RawDb, rows: &[T]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, rows, &now).await?;
    tx.commit().await?;
    Ok(())
}

async fn record_error(db: &RawDb, table: &str, id: &str, err: &str) -> Result<()> {
    let mut tx = db.pool().begin().await?;
    dr::record_object_error(&mut tx, table, id, err).await?;
    tx.commit().await?;
    Ok(())
}

fn id_of(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match &v[*k] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn str_of(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        cur = &cur[*k];
    }
    cur.as_str().map(str::to_string)
}

/// How one day's request came out.
enum Day {
    Fetched(DailyRow),
    Failed,
    /// The request failed because the step was told to stop. Not a
    /// failure of the day: nothing is recorded.
    Stopped,
}

#[derive(Debug, PartialEq, Eq)]
enum DaysEnd {
    /// Every day was tried, or the metric was given up on.
    Done,
    Stopped,
}

impl Walk<'_> {
    fn stopping(&self) -> bool {
        self.stop.requested()
    }

    async fn wrote(&self, rows: u64) {
        if let Some(sealer) = self.sealer {
            sealer.wrote(rows).await;
        }
    }

    /// One listing request. Only an auth failure is an `Err`; any other
    /// way of not getting an array comes back as the reason.
    async fn list_once(
        &mut self,
        path: &str,
        wrapped: bool,
    ) -> Result<std::result::Result<Vec<Value>, String>> {
        match self.client.get_json(path).await {
            Ok(fetched) => Ok(listing_array(fetched, wrapped)),
            Err(e) if is_auth(&e) => Err(e),
            Err(e) => Ok(Err(format!("{e:#}"))),
        }
    }

    /// A `start`/`limit` listing, `page(offset)` naming each page's
    /// path, walked to its first short page. A page that is not an
    /// array, or whose request failed, ends the walk incomplete; so does
    /// a walk that never reaches a short page.
    async fn list_pages(
        &mut self,
        page: impl Fn(usize) -> String,
        page_size: usize,
        wrapped: bool,
    ) -> Result<Listed> {
        let mut rows: Vec<Value> = Vec::new();
        let mut offset = 0usize;
        for _ in 0..MAX_LISTING_PAGES {
            match self.list_once(&page(offset), wrapped).await? {
                Ok(page_rows) => {
                    let n = page_rows.len();
                    // An endpoint that ignores `start` answers the same
                    // full page forever; one repeat is enough to know.
                    if n > 0 && rows.ends_with(&page_rows) {
                        return Ok(Listed {
                            rows,
                            incomplete: Some(format!(
                                "page at offset {offset} repeated the page before it"
                            )),
                        });
                    }
                    rows.extend(page_rows);
                    if n < page_size {
                        return Ok(Listed {
                            rows,
                            incomplete: None,
                        });
                    }
                    offset += n;
                }
                Err(why) => {
                    return Ok(Listed {
                        rows,
                        incomplete: Some(format!("page at offset {offset}: {why}")),
                    })
                }
            }
        }
        Ok(Listed {
            rows,
            incomplete: Some(format!("still a full page after {MAX_LISTING_PAGES} pages")),
        })
    }

    /// A listing that is not an enumeration: nothing is pruned to it
    /// this run, and the run says so. Counted once per listing.
    fn listing_failed(&mut self, s: &mut FetchSummary, name: &str, why: &str) {
        if self.stopping() {
            info!(
                event = "garmin_listing_stopped",
                listing = name,
                reason = why,
                "a listing ended on the stop; nothing is pruned"
            );
            return;
        }
        s.errors += 1;
        let problem = RunProblem::listing(name, why);
        let counted = self.found.run_problems();
        if counted.iter().any(|p| p.key() == problem.key()) {
            return;
        }
        s.listings_failed += 1;
        self.found.push(problem);
    }

    // ── devices ──────────────────────────────────────────────────────

    async fn devices(&mut self, s: &mut FetchSummary) -> Result<()> {
        let list = match self
            .list_once("/device-service/deviceregistration/devices", false)
            .await?
        {
            Ok(list) => list,
            Err(why) => {
                self.listing_failed(s, "devices", &why);
                return Ok(());
            }
        };
        let mut rows = Vec::with_capacity(list.len());
        for d in &list {
            let Some(id) = id_of(d, &["deviceId", "unitId"]) else {
                warn!(
                    event = "garmin_device_without_id",
                    "skipping a device with no deviceId"
                );
                continue;
            };
            rows.push(DeviceRow {
                id_and_payload: WirePayload {
                    id,
                    payload: d.to_string(),
                },
                product_display_name: str_of(d, &["productDisplayName"]),
            });
        }
        let keep: HashSet<&str> = rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
        upsert(self.db, &rows).await?;
        self.db.prune_devices(&keep).await?;
        s.devices = rows.len();
        self.wrote(rows.len() as u64).await;
        Ok(())
    }

    // ── per-day metrics ──────────────────────────────────────────────

    async fn daily(&mut self, s: &mut FetchSummary) -> Result<()> {
        let forgotten = self
            .db
            .forget_daily_problems_outside(&ymd(self.since), &ymd(self.end))
            .await?;
        if forgotten > 0 {
            info!(event = "garmin_failed_days_forgotten", forgotten, since = %ymd(self.since), end = %ymd(self.end), "failed days outside the window no longer count as problems");
        }
        let mut walks = Vec::new();
        for metric in self.api.metrics() {
            let held = self.db.daily_held(metric).await?;
            walks.push((metric, owed_days(self.since, self.end, &held, self.refresh)));
        }
        let total: usize = walks.iter().map(|(_, days)| days.len()).sum();
        self.progress.set_length(Some(total as u64));
        for (metric, days) in walks {
            info!(
                event = "garmin_daily_begin",
                metric,
                days = days.len(),
                "walking one daily metric"
            );
            if self.walk_days(metric, &days, s).await? == DaysEnd::Stopped {
                return Ok(());
            }
            s.metrics += 1;
        }
        Ok(())
    }

    /// Fetch `days` of one metric, in order. A day is one request; a
    /// month of them is one write, so a run that dies re-fetches at most
    /// a month.
    async fn walk_days(
        &mut self,
        metric: &str,
        days: &[NaiveDate],
        s: &mut FetchSummary,
    ) -> Result<DaysEnd> {
        let mut pending: Vec<DailyRow> = Vec::new();
        let mut consecutive_failures = 0u32;
        let mut end = DaysEnd::Done;
        for (i, day) in days.iter().enumerate() {
            match self.fetch_day(metric, *day, s).await? {
                Day::Stopped => {
                    end = DaysEnd::Stopped;
                    break;
                }
                Day::Fetched(row) => {
                    pending.push(row);
                    consecutive_failures = 0;
                }
                Day::Failed => consecutive_failures += 1,
            }
            self.progress.inc(1);
            if consecutive_failures >= CONSECUTIVE_FAILURE_BUDGET {
                warn!(
                    event = "garmin_metric_abandoned",
                    metric,
                    after = consecutive_failures,
                    "too many days in a row failed; leaving the rest of this metric for the next run"
                );
                self.progress.inc((days.len() - i - 1) as u64);
                break;
            }
            if pending.len() >= DAILY_BATCH_DAYS {
                self.flush_daily(&mut pending).await?;
            }
        }
        self.flush_daily(&mut pending).await?;
        Ok(end)
    }

    async fn fetch_day(
        &mut self,
        metric: &str,
        day: NaiveDate,
        s: &mut FetchSummary,
    ) -> Result<Day> {
        let d = ymd(day);
        self.progress.set_message(&format!("garmin: {metric} {d}"));
        let path = daily_path(metric, &self.account.display_name, &d);
        let id = DailyRow::id_for(metric, &d);
        match self.client.get_json(&path).await {
            Ok(fetched) => {
                let payload = match fetched {
                    Fetched::Some(v) if !is_empty_answer(&v) => v.to_string(),
                    _ => "null".to_string(),
                };
                s.days += 1;
                Ok(Day::Fetched(DailyRow {
                    id_and_payload: WirePayload { id, payload },
                    metric: metric.to_string(),
                    calendar_date: d,
                    fetched_on: Some(ymd(self.today)),
                }))
            }
            Err(e) if is_auth(&e) => Err(e),
            Err(_) if self.stopping() => Ok(Day::Stopped),
            Err(e) => {
                s.errors += 1;
                record_error(self.db, "garmin_daily", &id, &format!("{e:#}")).await?;
                Ok(Day::Failed)
            }
        }
    }

    async fn flush_daily(&mut self, pending: &mut Vec<DailyRow>) -> Result<()> {
        upsert(self.db, pending).await?;
        self.wrote(pending.len() as u64).await;
        pending.clear();
        Ok(())
    }

    // ── weigh-ins ────────────────────────────────────────────────────

    async fn weight(&mut self, s: &mut FetchSummary) -> Result<()> {
        let mut chunk_start = self.since;
        let mut seen: HashSet<String> = HashSet::new();
        let mut complete = true;
        while chunk_start <= self.end && !self.stopping() {
            let chunk_end = (chunk_start + Duration::days(WEIGHT_CHUNK_DAYS - 1)).min(self.end);
            let path = format!(
                "/weight-service/weight/range/{}/{}?includeAll=true",
                ymd(chunk_start),
                ymd(chunk_end)
            );
            let listed = match self.client.get_json(&path).await {
                Ok(Fetched::Some(v)) => match v["dailyWeightSummaries"].as_array() {
                    Some(summaries) => Ok(weigh_in_rows(summaries)),
                    None => {
                        let preview: String = v.to_string().chars().take(120).collect();
                        Err(format!("no dailyWeightSummaries array: {preview}"))
                    }
                },
                Ok(Fetched::Nothing) => Err(NOTHING.to_string()),
                Err(e) if is_auth(&e) => return Err(e),
                Err(e) => Err(format!("{e:#}")),
            };
            match listed {
                Ok(rows) => {
                    for r in &rows {
                        seen.insert(r.id_and_payload.id.clone());
                    }
                    s.weigh_ins += rows.len();
                    upsert(self.db, &rows).await?;
                    self.wrote(rows.len() as u64).await;
                }
                Err(why) => {
                    complete = false;
                    let why = format!("{}..{}: {why}", ymd(chunk_start), ymd(chunk_end));
                    self.listing_failed(s, "weight", &why);
                }
            }
            chunk_start = chunk_end + Duration::days(1);
        }
        // A chunk that did not list leaves the window unenumerated: no
        // prune.
        if !complete || self.stopping() {
            return Ok(());
        }
        let keep: HashSet<&str> = seen.iter().map(String::as_str).collect();
        s.weigh_ins_pruned = self
            .db
            .prune_weigh_ins(&ymd(self.since), &ymd(self.end), &keep)
            .await?;
        Ok(())
    }

    // ── activities ───────────────────────────────────────────────────

    async fn activities(&mut self, s: &mut FetchSummary) -> Result<()> {
        let want = Span::new(ymd(self.since), ymd(self.end));
        let held = coverage::held(self.db.pool(), ACTIVITIES_SCOPE).await?;
        let start = listing_start(&want, &held, self.end - self.refresh)?;
        let start_date = ymd(start);
        let Listed {
            rows: listed,
            incomplete,
        } = self
            .list_pages(
                |offset| {
                    format!(
                        "/activitylist-service/activities/search/activities?start={offset}&limit={ACTIVITY_PAGE}&startDate={start_date}"
                    )
                },
                ACTIVITY_PAGE,
                false,
            )
            .await?;
        if let Some(why) = &incomplete {
            self.listing_failed(s, "activities", why);
        }
        s.activities_listed = listed.len();

        let mut rows = Vec::with_capacity(listed.len());
        for a in listed {
            let Some(id) = id_of(&a, &["activityId"]) else {
                continue;
            };
            let payload = a.to_string();
            rows.push(ActivityRow {
                start_time_gmt: str_of(&a, &["startTimeGMT"]),
                activity_type: str_of(&a, &["activityType", "typeKey"]),
                name: str_of(&a, &["activityName"]),
                listing_hash: Some(blake3_hex(payload.as_bytes())),
                id_and_payload: WirePayload { id, payload },
            });
        }
        // A listing that reached its end names every activity from
        // `start` on, so one dated clearly inside it that it did not name
        // is gone upstream. A day of slack keeps the local/GMT boundary
        // out of it. One that stopped short still stores the pages it
        // got, and neither prunes nor counts as having looked.
        let enumerated = incomplete.is_none().then(|| Enumerated {
            covered: Span::new(start_date.clone(), ymd(self.end)),
            prune_from: format!("{} 00:00:00", ymd(start + Duration::days(1))),
        });
        s.activities_pruned = self
            .db
            .store_activity_listing(&rows, enumerated.as_ref())
            .await?;
        self.wrote(rows.len() as u64).await;
        self.activity_details_and_files(s).await
    }

    /// Fetch what the stored activities are owed, whichever run listed
    /// them.
    async fn activity_details_and_files(&mut self, s: &mut FetchSummary) -> Result<()> {
        let files_on = self.api.activity_files();
        let work: Vec<ActivityWork> = self
            .db
            .activity_work()
            .await?
            .into_iter()
            .map(|w| ActivityWork {
                file: w.file && files_on,
                ..w
            })
            .filter(|w| w.detail || w.file)
            .collect();
        self.progress.set_length(Some(work.len() as u64));
        let mut files = FitBatch::default();
        for w in &work {
            if self.stopping() {
                break;
            }
            let id = &w.id;
            self.progress.set_message(&format!("garmin: activity {id}"));
            if w.detail {
                let detail = match self
                    .client
                    .get_json(&format!("/activity-service/activity/{id}"))
                    .await
                {
                    Ok(Fetched::Some(detail)) => {
                        s.activities_fetched += 1;
                        Some(detail.to_string())
                    }
                    // Garmin has none: held as `null` for this version of
                    // the listing, so it is not asked for again until
                    // the activity changes.
                    Ok(Fetched::Nothing) => Some("null".to_string()),
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(_) if self.stopping() => break,
                    Err(e) => {
                        s.errors += 1;
                        record_error(self.db, "garmin_activity_details", id, &format!("{e:#}"))
                            .await?;
                        None
                    }
                };
                if let Some(payload) = detail {
                    upsert(
                        self.db,
                        &[ActivityDetailRow {
                            id_and_payload: WirePayload {
                                id: id.clone(),
                                payload,
                            },
                            listing_hash: Some(w.listing_hash.clone()),
                        }],
                    )
                    .await?;
                }
            }
            if w.file {
                let fit_ref = file_ref(id, FILE_KIND_FIT);
                match self
                    .client
                    .get_bytes(&format!("/download-service/files/activity/{id}"))
                    .await
                {
                    Ok(Fetched::Some(zip)) => {
                        files
                            .answered_for
                            .insert(id.clone(), w.listing_hash.clone());
                        match fit_from_zip(&zip) {
                            Ok(fit) => {
                                files.edges.add_fetched(
                                    id,
                                    &fit_ref,
                                    fit,
                                    Some("application/vnd.ant.fit".into()),
                                    Some(format!("{id}_ACTIVITY.fit")),
                                );
                                s.activity_files += 1;
                            }
                            // The same bytes would come back, so this is
                            // an answer for this version of the listing,
                            // and a problem until the activity changes.
                            Err(e) => {
                                s.errors += 1;
                                files.edges.add_failed(
                                    id,
                                    &fit_ref,
                                    format!("the download held no readable FIT file: {e:#}"),
                                );
                            }
                        }
                    }
                    // An activity entered by hand has no file.
                    Ok(Fetched::Nothing) => {
                        upsert(
                            self.db,
                            &[ActivityFileRow {
                                id: fit_ref,
                                activity_id: id.clone(),
                                file_kind: FILE_KIND_FIT.to_string(),
                                blake3: None,
                                listing_hash: Some(w.listing_hash.clone()),
                            }],
                        )
                        .await?;
                    }
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(_) if self.stopping() => break,
                    Err(e) => {
                        s.errors += 1;
                        files.edges.add_failed(id, &fit_ref, format!("{e:#}"));
                    }
                }
            }
            self.progress.inc(1);
            if files.edges.bundle_mut().len() >= FILE_BATCH {
                std::mem::take(&mut files).flush(self.db).await?;
                self.wrote(FILE_BATCH as u64).await;
            }
        }
        files.flush(self.db).await
    }

    // ── wellness FIT bundles ─────────────────────────────────────────

    async fn wellness(&mut self, s: &mut FetchSummary) -> Result<()> {
        if !self.api.wellness_files() {
            return Ok(());
        }
        let held = self.db.wellness_held().await?;
        let days = owed_days(self.since, self.end, &held, self.refresh);
        self.progress.set_length(Some(days.len() as u64));
        let mut batch = WellnessBatch::default();
        for day in days {
            if self.stopping() || self.wellness_day(day, &mut batch, s).await? == DaysEnd::Stopped {
                break;
            }
            self.progress.inc(1);
            if batch.pending() >= FILE_BATCH {
                let wrote = batch.pending() as u64;
                std::mem::take(&mut batch)
                    .flush(self.db, self.today)
                    .await?;
                self.wrote(wrote).await;
            }
        }
        batch.flush(self.db, self.today).await
    }

    async fn wellness_day(
        &mut self,
        day: NaiveDate,
        batch: &mut WellnessBatch,
        s: &mut FetchSummary,
    ) -> Result<DaysEnd> {
        let d = ymd(day);
        let zip_ref = file_ref(&d, FILE_KIND_WELLNESS_ZIP);
        self.progress.set_message(&format!("garmin: wellness {d}"));
        match self
            .client
            .get_bytes(&format!("/download-service/files/wellness/{d}"))
            .await
        {
            Ok(Fetched::Some(zip)) => {
                batch.edges.add_fetched(
                    &d,
                    &zip_ref,
                    zip,
                    Some("application/zip".into()),
                    Some(format!("{d}_wellness.zip")),
                );
                s.wellness_files += 1;
            }
            Ok(Fetched::Nothing) => batch.without_a_bundle.push(d),
            Err(e) if is_auth(&e) => return Err(e),
            Err(_) if self.stopping() => return Ok(DaysEnd::Stopped),
            Err(e) => {
                s.errors += 1;
                batch.edges.add_failed(&d, &zip_ref, format!("{e:#}"));
            }
        }
        Ok(DaysEnd::Done)
    }

    // ── whole-account listings ───────────────────────────────────────

    async fn items(&mut self, s: &mut FetchSummary) -> Result<()> {
        let display_name = self.account.display_name.clone();
        let profile_pk = self.account.profile_pk.clone();
        for kind in ITEM_KINDS {
            if self.stopping() {
                break;
            }
            let path = |offset: usize| {
                item_listing_path(kind.name, &display_name, profile_pk.as_deref(), offset)
            };
            let Some(first_page) = path(0) else {
                // Not counted in `errors`: it is the account's shape, the
                // same every run.
                self.found.listing(
                    kind.name,
                    "socialProfile carries no profileId, which the gear listing is keyed on",
                );
                continue;
            };
            let Listed {
                rows: list,
                incomplete,
            } = if kind.paged {
                self.list_pages(
                    |offset| path(offset).expect("the first page resolved"),
                    ITEM_PAGE,
                    true,
                )
                .await?
            } else {
                match self.list_once(&first_page, true).await? {
                    Ok(rows) => Listed {
                        rows,
                        incomplete: None,
                    },
                    Err(why) => Listed {
                        rows: Vec::new(),
                        incomplete: Some(why),
                    },
                }
            };
            if let Some(why) = &incomplete {
                self.listing_failed(s, kind.name, why);
            }
            let mut rows = Vec::with_capacity(list.len());
            for item in &list {
                let Some(upstream_id) = id_of(item, kind.id_keys) else {
                    warn!(
                        event = "garmin_item_without_id",
                        kind = kind.name,
                        "skipping an item with no id"
                    );
                    continue;
                };
                rows.push(ItemRow {
                    id_and_payload: WirePayload {
                        id: ItemRow::id_for(kind.name, &upstream_id),
                        payload: item.to_string(),
                    },
                    kind: kind.name.to_string(),
                    upstream_id,
                });
            }
            let keep: HashSet<&str> = rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
            upsert(self.db, &rows).await?;
            s.items += rows.len();
            if incomplete.is_none() {
                s.items_pruned += self.db.prune_items(kind.name, &keep).await?;
            }
            self.wrote(rows.len() as u64).await;
        }
        Ok(())
    }
}

/// The CAS bundle keys its blobs by ref id, so each record's file needs
/// a ref of its own; the edge row's id is that ref.
fn file_ref(owner: &str, file_kind: &str) -> String {
    format!("{owner}#{file_kind}")
}

/// The FIT downloads of one transaction.
#[derive(Default)]
struct FitBatch {
    edges: CasEdgeAccumulator,
    /// `activity id → listing version` for each download Garmin
    /// answered, readable or not. A request that failed is absent, so
    /// its edge carries no version and the file stays owed.
    answered_for: HashMap<String, String>,
}

impl FitBatch {
    async fn flush(self, db: &RawDb) -> Result<()> {
        self.edges
            .flush(db.pool(), db.cas(), |activity_id, file_ref, blake3| {
                ActivityFileRow {
                    id: file_ref.to_string(),
                    activity_id: activity_id.to_string(),
                    file_kind: FILE_KIND_FIT.to_string(),
                    blake3: blake3.map(str::to_string),
                    listing_hash: self.answered_for.get(activity_id).cloned(),
                }
            })
            .await
    }
}

/// The wellness days of one transaction.
#[derive(Default)]
struct WellnessBatch {
    edges: CasEdgeAccumulator,
    /// Days Garmin answered with no bundle.
    without_a_bundle: Vec<String>,
}

impl WellnessBatch {
    fn pending(&mut self) -> usize {
        self.edges.bundle_mut().len() + self.without_a_bundle.len()
    }

    /// A day is stamped with the date it was fetched on only when Garmin
    /// answered for it: a bundle, or none. A failed request leaves the
    /// stamp empty and the day owed.
    async fn flush(self, db: &RawDb, today: NaiveDate) -> Result<()> {
        let fetched_on = Some(ymd(today));
        let row = |day: &str, blake3: Option<&str>, answered: bool| WellnessFileRow {
            id: file_ref(day, FILE_KIND_WELLNESS_ZIP),
            calendar_date: day.to_string(),
            file_kind: FILE_KIND_WELLNESS_ZIP.to_string(),
            blake3: blake3.map(str::to_string),
            fetched_on: fetched_on.clone().filter(|_| answered),
        };
        let none: Vec<WellnessFileRow> = self
            .without_a_bundle
            .iter()
            .map(|day| row(day, None, true))
            .collect();
        upsert(db, &none).await?;
        self.edges
            .flush(db.pool(), db.cas(), |day, _, blake3| {
                row(day, blake3, blake3.is_some())
            })
            .await
    }
}

fn is_auth(e: &anyhow::Error) -> bool {
    e.downcast_ref::<GarminError>()
        .is_some_and(|g| matches!(g, GarminError::Auth(_)))
}

/// Garmin's "no data for that day" comes in several spellings.
fn is_empty_answer(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// The path of one metric for one day. `display_name` is only
/// interpolated where the service keys on the user rather than on the
/// bearer.
pub fn daily_path(metric: &str, display_name: &str, d: &str) -> String {
    let dn = urlencoding::encode(display_name);
    match metric {
        "daily_summary" => format!("/usersummary-service/usersummary/daily/{dn}?calendarDate={d}"),
        "heart_rate" => format!("/wellness-service/wellness/dailyHeartRate/{dn}?date={d}"),
        "sleep" => format!(
            "/wellness-service/wellness/dailySleepData/{dn}?date={d}&nonSleepBufferMinutes=60"
        ),
        "stress" => format!("/wellness-service/wellness/dailyStress/{d}"),
        "body_battery" => format!(
            "/wellness-service/wellness/bodyBattery/reports/daily?startDate={d}&endDate={d}"
        ),
        "body_battery_events" => format!("/wellness-service/wellness/bodyBattery/events/{d}"),
        "respiration" => format!("/wellness-service/wellness/daily/respiration/{d}"),
        "spo2" => format!("/wellness-service/wellness/daily/spo2/{d}"),
        "hrv" => format!("/hrv-service/hrv/{d}"),
        "training_readiness" => format!("/metrics-service/metrics/trainingreadiness/{d}"),
        "training_status" => format!("/metrics-service/metrics/trainingstatus/aggregated/{d}"),
        "intensity_minutes" => format!("/wellness-service/wellness/daily/im/{d}"),
        "floors" => format!("/wellness-service/wellness/floorsChartData/daily/{d}"),
        "hydration" => format!("/usersummary-service/usersummary/hydration/daily/{d}"),
        "steps_chart" => format!("/wellness-service/wellness/dailySummaryChart/{dn}?date={d}"),
        "fitness_age" => format!("/fitnessage-service/fitnessage/{d}"),
        "max_metrics" => format!("/metrics-service/metrics/maxmet/daily/{d}/{d}"),
        "daily_events" => format!("/wellness-service/wellness/dailyEvents?calendarDate={d}"),
        "endurance_score" => format!("/metrics-service/metrics/endurancescore?calendarDate={d}"),
        "hill_score" => format!("/metrics-service/metrics/hillscore?calendarDate={d}"),
        other => panic!("daily_path: {other:?} is not in DAILY_METRICS, which validate() checks"),
    }
}

/// `dailyWeightSummaries[].allWeightMetrics[]` flattened, one row per
/// weigh-in. The caller has already established that the summaries
/// array is there: its absence is not an empty range.
pub fn weigh_in_rows(summaries: &[Value]) -> Vec<WeighInRow> {
    let mut out = Vec::new();
    for summary in summaries {
        let Some(metrics) = summary["allWeightMetrics"].as_array() else {
            continue;
        };
        for m in metrics {
            let Some(id) = id_of(m, &["samplePk"]) else {
                warn!(
                    event = "garmin_weigh_in_without_pk",
                    "skipping a weigh-in with no samplePk"
                );
                continue;
            };
            out.push(WeighInRow {
                id_and_payload: WirePayload {
                    id,
                    payload: m.to_string(),
                },
                calendar_date: str_of(m, &["calendarDate"]),
                timestamp_gmt: m["timestampGMT"].as_i64(),
                weight_g: m["weight"].as_f64(),
                source_type: str_of(m, &["sourceType"]),
            });
        }
    }
    out
}

/// `/download-service/files/activity/<id>` is a zip holding one
/// `<id>_ACTIVITY.fit`; the FIT file is what gets stored.
pub fn fit_from_zip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).context("activity download is not a zip")?;
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..zip.len() {
        let name = zip.by_index(i)?.name().to_string();
        names.insert(name, i);
    }
    let (name, idx) = names
        .iter()
        .find(|(n, _)| n.to_ascii_lowercase().ends_with(".fit"))
        .or_else(|| names.iter().next())
        .map(|(n, i)| (n.clone(), *i))
        .ok_or_else(|| anyhow!("activity zip is empty"))?;
    let mut out = Vec::new();
    zip.by_index(idx)
        .with_context(|| format!("read {name} from activity zip"))?
        .read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A window that ends in the past stays put; one ending in the
    /// future cannot walk past today.
    #[test]
    fn the_walk_ends_at_until_or_today_whichever_is_first() {
        let today = date("2026-09-24").unwrap();
        assert_eq!(walk_end(today, None).unwrap(), today);
        assert_eq!(
            walk_end(today, Some("2025-08-31")).unwrap(),
            date("2025-08-31").unwrap()
        );
        assert_eq!(walk_end(today, Some("2027-01-01")).unwrap(), today);
        assert!(walk_end(today, Some("soon")).is_err());
    }

    #[test]
    fn every_declared_metric_has_a_path() {
        for m in datalib_etl_garmin_config::DAILY_METRICS {
            let p = daily_path(m, "Some Body", "2026-09-14");
            assert!(p.starts_with('/'), "{m}: {p}");
            assert!(p.contains("2026-09-14"), "{m}: {p}");
            assert!(!p.contains(' '), "{m}: display name must be encoded: {p}");
        }
    }

    #[test]
    fn the_window_starts_at_since_else_the_first_runs_default() {
        let today = date("2026-09-28").unwrap();
        assert_eq!(
            window_start(Some("2026-01-01"), Some("2025-09-23"), today),
            "2026-01-01"
        );
        assert_eq!(window_start(None, Some("2025-09-23"), today), "2025-09-23");
        assert_eq!(window_start(None, None, today), "2025-09-28");
    }

    /// A day is asked for until a fetch lands on or after the date it
    /// settles; a day with no row is asked for however old; a day never
    /// settles on its own date, whatever the refresh window.
    #[test]
    fn a_day_is_owed_until_it_is_fetched_after_it_settled() {
        let d = |s| date(s).unwrap();
        let week = Duration::days(7);
        let held: HashMap<String, String> = [
            ("2369-04-01", "2369-04-08"),
            ("2369-04-02", "2369-04-08"),
            ("2369-04-04", "2369-04-15"),
            ("2369-04-08", "2369-04-15"),
            ("2369-04-09", "2369-04-15"),
        ]
        .map(|(day, on)| (day.to_string(), on.to_string()))
        .into();
        assert_eq!(
            owed_days(d("2369-04-01"), d("2369-04-10"), &held, week),
            [
                d("2369-04-02"),
                d("2369-04-03"),
                d("2369-04-05"),
                d("2369-04-06"),
                d("2369-04-07"),
                d("2369-04-09"),
                d("2369-04-10"),
            ]
        );
        let same_day: HashMap<String, String> =
            [("2369-04-15".to_string(), "2369-04-15".to_string())].into();
        assert_eq!(
            owed_days(
                d("2369-04-15"),
                d("2369-04-15"),
                &same_day,
                Duration::zero()
            ),
            [d("2369-04-15")]
        );
        assert!(owed_days(d("2369-04-09"), d("2369-04-08"), &held, week).is_empty());
    }

    /// The listing starts at the lowest date nobody has listed — a
    /// `since` moved earlier is one — and otherwise at the refresh
    /// window, never before `since`.
    #[test]
    fn the_activity_listing_starts_at_the_lowest_gap_or_the_refresh_window() {
        let d = |s| date(s).unwrap();
        let want = Span::new("2369-04-01", "2369-04-15");
        let refresh_from = d("2369-04-08");
        let start = |held: &[Span]| listing_start(&want, held, refresh_from).unwrap();
        assert_eq!(start(&[]), d("2369-04-01"));
        assert_eq!(
            start(&[Span::new("2369-04-01", "2369-04-15")]),
            refresh_from
        );
        assert_eq!(
            start(&[Span::new("2369-04-05", "2369-04-15")]),
            d("2369-04-01"),
            "a since moved earlier leaves a gap below what was listed"
        );
        assert_eq!(
            start(&[Span::new("2369-04-01", "2369-04-12")]),
            refresh_from,
            "the gap since the last run lies inside the refresh window"
        );
        assert_eq!(
            start(&[Span::new("2369-04-01", "2369-04-03")]),
            d("2369-04-03"),
            "a gap below the refresh window is listed from its start"
        );
        let narrow = Span::new("2369-04-12", "2369-04-15");
        assert_eq!(
            listing_start(&narrow, std::slice::from_ref(&narrow), refresh_from).unwrap(),
            d("2369-04-12")
        );
    }

    #[test]
    fn weigh_ins_flatten_the_range_reply() {
        let v = json!({
            "dailyWeightSummaries": [
                {"summaryDate": "2026-09-13", "allWeightMetrics": [
                    {"samplePk": 1, "calendarDate": "2026-09-13", "weight": 80000, "timestampGMT": 1_757_700_000_000i64, "sourceType": "INDEX_SCALE"},
                    {"samplePk": 2, "calendarDate": "2026-09-13", "weight": 80100, "timestampGMT": 1_757_760_000_000i64, "sourceType": "MANUAL"}
                ]},
                {"summaryDate": "2026-09-14", "allWeightMetrics": []}
            ]
        });
        let rows = weigh_in_rows(v["dailyWeightSummaries"].as_array().unwrap());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id_and_payload.id, "1");
        assert_eq!(rows[1].weight_g, Some(80100.0));
        assert_eq!(rows[1].source_type.as_deref(), Some("MANUAL"));
    }

    #[test]
    fn fit_is_pulled_out_of_the_zip() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("123_ACTIVITY.fit", opts).unwrap();
            w.write_all(b".FIT-bytes").unwrap();
            w.finish().unwrap();
        }
        assert_eq!(fit_from_zip(&buf.into_inner()).unwrap(), b".FIT-bytes");
        assert!(fit_from_zip(b"not a zip").is_err());
    }

    /// Only an array is an enumeration. An empty array is one — "you
    /// have no badges" — and a wrapped object is one only when it
    /// wraps an array.
    #[test]
    fn only_an_array_is_an_enumeration() {
        let arr = |v: Value| listing_array(Fetched::Some(v), false);
        assert_eq!(arr(json!([])).unwrap(), Vec::<Value>::new());
        assert_eq!(arr(json!([1, 2])).unwrap().len(), 2);
        assert!(
            arr(json!({"list": [1]})).is_err(),
            "bare kinds take no wrapper"
        );
        assert!(arr(json!("x")).unwrap_err().contains("expected an array"));
        assert!(listing_array(Fetched::Nothing, false)
            .unwrap_err()
            .contains("nothing"));

        let wrapped = |v: Value| listing_array(Fetched::Some(v), true);
        assert_eq!(wrapped(json!({"count": 1, "list": [1]})).unwrap().len(), 1);
        assert!(wrapped(json!({"count": 0}))
            .unwrap_err()
            .contains("no array inside"));
        assert!(wrapped(json!({})).is_err(), "an empty object wraps nothing");
    }

    #[test]
    fn every_item_kind_has_a_path_and_only_the_paged_ones_take_an_offset() {
        for kind in ITEM_KINDS {
            let p0 = item_listing_path(kind.name, "Some Body", Some("1701"), 0).unwrap();
            let p1 = item_listing_path(kind.name, "Some Body", Some("1701"), ITEM_PAGE).unwrap();
            assert!(p0.starts_with('/'), "{}: {p0}", kind.name);
            assert!(
                !p0.contains(' '),
                "{}: display name must be encoded: {p0}",
                kind.name
            );
            assert_eq!(p0 != p1, kind.paged, "{}: {p0} vs {p1}", kind.name);
        }
        assert!(item_listing_path("gear", "x", None, 0).is_none());
    }

    #[test]
    fn empty_answers_are_recognised() {
        assert!(is_empty_answer(&json!(null)));
        assert!(is_empty_answer(&json!([])));
        assert!(is_empty_answer(&json!({})));
        assert!(!is_empty_answer(&json!({"a": 1})));
        assert!(!is_empty_answer(&json!(0)));
    }
}
