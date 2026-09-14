//! Garmin Connect → doltlite. Five walks over one account, each with
//! its own cursor: the per-day metrics, the weigh-ins, the activities
//! (listing, detail and FIT file), the optional per-day wellness FIT
//! bundle, and the small whole-account listings. Garmin has no "what
//! changed since" API, so incrementality is a trailing re-fetch window
//! per walk, and UPSERT-by-upstream-id makes the overlap free.

pub mod api;
pub mod db;
pub mod schema_raw;

use std::collections::{BTreeMap, HashSet};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration, NaiveDate};
use serde_json::{json, Value};
use tracing::{info, warn};

use datalib_etl::blob_cas::CasEdgeAccumulator;
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl_garmin_config::{GarminApi, DEFAULT_SINCE_DAYS};

use crate::auth::Credentials;
use api::{Fetched, GarminClient, GarminError};
pub use db::{db_path_for, RawDb};
use schema_raw::*;

pub const SCOPE_CONFIG_KEY: &str = "garmin:download";
const CURSOR_DAILY_PREFIX: &str = "garmin:daily:";
const CURSOR_WEIGHT: &str = "garmin:weight";
const CURSOR_ACTIVITIES: &str = "garmin:activities";
const CURSOR_WELLNESS: &str = "garmin:wellness";

/// A metric that fails this many days in a row is abandoned for the
/// run: a dead endpoint should not cost a request per day of history.
const CONSECUTIVE_FAILURE_BUDGET: u32 = 10;
/// `/weight-service/weight/range/<start>/<end>` per request.
pub const WEIGHT_CHUNK_DAYS: i64 = 90;
pub const ACTIVITY_PAGE: usize = 100;
/// Pages of the activity listing before the walk is declared runaway.
const MAX_ACTIVITY_PAGES: usize = 2000;

pub const ITEM_KINDS: &[&str] = &["personal_records", "gear", "badges", "workouts", "goals"];

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub creds: Credentials,
    pub api: GarminApi,
    /// The run's local calendar date: the last day every walk reaches.
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
    pub errors: usize,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "requests={} metrics={} days={} weigh_ins={} weigh_ins_pruned={} \
             activities_listed={} activities_fetched={} activities_pruned={} \
             activity_files={} wellness_files={} devices={} items={} items_pruned={} errors={}",
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
            self.errors,
        )
    }
}

/// The account facts every per-user path needs, read once per run.
struct Account {
    display_name: String,
    profile_pk: Option<String>,
}

fn scope_config_blob(api: &GarminApi, since: &str) -> Value {
    json!({
        "since": since,
        "metrics": api.metrics(),
        "activity_files": api.activity_files(),
        "wellness_files": api.wellness_files(),
    })
}

pub fn date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("date {s:?}"))
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db;
    let api = opts.api;
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    if opts.control.refetch_blobs {
        db.clear_blob_hashes().await?;
    }
    let since_str = match &api.since {
        Some(s) => s.clone(),
        None => ymd(opts.today - Duration::days(DEFAULT_SINCE_DAYS)),
    };
    let since = date(&since_str)?;
    let scope_cfg = scope_config_blob(&api, &since_str);
    let prior = datalib_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;
    // A `since` earlier than the one every cursor was walked under means
    // the range below the old start was never fetched: ignore the
    // cursors and walk from the new start.
    let since_widened = prior
        .as_ref()
        .and_then(|p| p.get("since"))
        .and_then(Value::as_str)
        .is_some_and(|p| since_str.as_str() < p);
    if since_widened {
        info!(event = "garmin_since_widened", to = %since_str, "re-walking every cursor from the new start");
    }
    let refresh = Duration::days(api.refresh_days());
    let mut client = GarminClient::new(opts.creds);
    let mut s = FetchSummary::default();
    let progress = &opts.progress;

    // Every phase after the account is independent, and one that fails
    // must not cost the others — except an auth failure, which they would
    // all share.
    let account = fetch_account(&mut client, &db).await?;
    let mut walk = Walk {
        db: &db,
        client: &mut client,
        account: &account,
        api: &api,
        since,
        today: opts.today,
        refresh,
        since_widened,
        sealer: opts.sealer.as_ref(),
        progress,
    };

    macro_rules! phase {
        ($name:literal, $fut:expr) => {
            progress.set_message(concat!("garmin: ", $name));
            if let Err(e) = $fut.await {
                if is_auth(&e) {
                    return Err(e.context(concat!("garmin ", $name)));
                }
                s.errors += 1;
                warn!(event = "garmin_phase_failed", phase = $name, error = %format!("{e:#}"));
            }
        };
    }
    phase!("devices", walk.devices(&mut s));
    phase!("daily", walk.daily(&mut s));
    phase!("weight", walk.weight(&mut s));
    phase!("activities", walk.activities(&mut s));
    phase!("wellness", walk.wellness(&mut s));
    phase!("items", walk.items(&mut s));
    let requests = walk.client.requests;
    s.requests = requests;
    datalib_etl::scope_config::store_if_satisfied(
        db.pool(),
        SCOPE_CONFIG_KEY,
        &scope_cfg,
        s.errors == 0,
    )
    .await;
    Ok(s)
}

/// Everything one run's walks share.
struct Walk<'a> {
    db: &'a RawDb,
    client: &'a mut GarminClient,
    account: &'a Account,
    api: &'a GarminApi,
    since: NaiveDate,
    today: NaiveDate,
    refresh: Duration,
    since_widened: bool,
    sealer: Option<&'a Sealer>,
    progress: &'a Progress,
}

async fn fetch_account(client: &mut GarminClient, db: &RawDb) -> Result<Account> {
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
    if let Fetched::Some(settings) = client
        .get_json("/userprofile-service/userprofile/user-settings")
        .await
        .context("garmin user-settings")?
    {
        rows.push(AccountRow {
            id_and_payload: WirePayload {
                id: ACCOUNT_USER_SETTINGS.into(),
                payload: settings.to_string(),
            },
            display_name: Some(display_name.clone()),
        });
    }
    upsert(db, &rows).await?;
    Ok(Account {
        display_name,
        profile_pk,
    })
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

impl Walk<'_> {
    /// Where a date walk resumes: `since`, or the cursor less the refresh
    /// window, whichever is later — unless `since` moved earlier.
    async fn resume_from(&self, scope: &str) -> Result<NaiveDate> {
        if self.since_widened {
            return Ok(self.since);
        }
        match self.db.cursor(scope).await? {
            Some(c) => {
                let c = date(&c)?;
                Ok((c - self.refresh).max(self.since))
            }
            None => Ok(self.since),
        }
    }

    async fn wrote(&self, rows: u64) {
        if let Some(sealer) = self.sealer {
            sealer.wrote(rows).await;
        }
    }

    // ── devices ──────────────────────────────────────────────────────

    async fn devices(&mut self, s: &mut FetchSummary) -> Result<()> {
        let list = match self
            .client
            .get_json("/device-service/deviceregistration/devices")
            .await?
        {
            Fetched::Some(Value::Array(a)) => a,
            Fetched::Some(other) => bail!("devices: expected an array, got {other}"),
            Fetched::Nothing => Vec::new(),
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
        let metrics = self.api.metrics();
        let total_days = (self.today - self.since).num_days().max(0) as u64 + 1;
        self.progress
            .set_length(Some(total_days * metrics.len() as u64));
        for metric in metrics {
            let scope = format!("{CURSOR_DAILY_PREFIX}{metric}");
            let start = self.resume_from(&scope).await?;
            let mut day = start;
            let mut consecutive_failures = 0u32;
            info!(event = "garmin_daily_begin", metric, start = %ymd(start), end = %ymd(self.today));
            while day <= self.today {
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
                        upsert(
                            self.db,
                            &[DailyRow {
                                id_and_payload: WirePayload { id, payload },
                                metric: metric.to_string(),
                                calendar_date: d.clone(),
                            }],
                        )
                        .await?;
                        consecutive_failures = 0;
                        s.days += 1;
                        self.wrote(1).await;
                    }
                    Err(e) => {
                        if is_auth(&e) {
                            return Err(e);
                        }
                        consecutive_failures += 1;
                        s.errors += 1;
                        warn!(event = "garmin_day_failed", metric, date = %d, error = %format!("{e:#}"));
                        record_error(self.db, "garmin_daily", &id, &format!("{e:#}")).await?;
                        if consecutive_failures >= CONSECUTIVE_FAILURE_BUDGET {
                            warn!(
                                event = "garmin_metric_abandoned",
                                metric,
                                after = consecutive_failures
                            );
                            break;
                        }
                    }
                }
                self.db.set_cursor(&scope, &d).await?;
                self.progress.inc(1);
                day += Duration::days(1);
            }
            s.metrics += 1;
        }
        Ok(())
    }

    // ── weigh-ins ────────────────────────────────────────────────────

    async fn weight(&mut self, s: &mut FetchSummary) -> Result<()> {
        let start = self.resume_from(CURSOR_WEIGHT).await?;
        let mut chunk_start = start;
        let mut seen: HashSet<String> = HashSet::new();
        while chunk_start <= self.today {
            let chunk_end = (chunk_start + Duration::days(WEIGHT_CHUNK_DAYS - 1)).min(self.today);
            let path = format!(
                "/weight-service/weight/range/{}/{}?includeAll=true",
                ymd(chunk_start),
                ymd(chunk_end)
            );
            let rows = match self.client.get_json(&path).await? {
                Fetched::Some(v) => weigh_in_rows(&v),
                Fetched::Nothing => Vec::new(),
            };
            for r in &rows {
                seen.insert(r.id_and_payload.id.clone());
            }
            s.weigh_ins += rows.len();
            upsert(self.db, &rows).await?;
            self.wrote(rows.len() as u64).await;
            chunk_start = chunk_end + Duration::days(1);
        }
        let keep: HashSet<&str> = seen.iter().map(String::as_str).collect();
        s.weigh_ins_pruned = self
            .db
            .prune_weigh_ins(&ymd(start), &ymd(self.today), &keep)
            .await?;
        self.db.set_cursor(CURSOR_WEIGHT, &ymd(self.today)).await?;
        Ok(())
    }

    // ── activities ───────────────────────────────────────────────────

    async fn activities(&mut self, s: &mut FetchSummary) -> Result<()> {
        let start = self.resume_from(CURSOR_ACTIVITIES).await?;
        let mut listed: Vec<Value> = Vec::new();
        let mut offset = 0usize;
        for _ in 0..MAX_ACTIVITY_PAGES {
            let path = format!(
                "/activitylist-service/activities/search/activities?start={offset}&limit={ACTIVITY_PAGE}&startDate={}",
                ymd(start)
            );
            let page = match self.client.get_json(&path).await? {
                Fetched::Some(Value::Array(a)) => a,
                Fetched::Some(other) => bail!("activities: expected an array, got {other}"),
                Fetched::Nothing => Vec::new(),
            };
            let n = page.len();
            listed.extend(page);
            if n < ACTIVITY_PAGE {
                break;
            }
            offset += n;
        }
        s.activities_listed = listed.len();

        let ids: Vec<String> = listed
            .iter()
            .filter_map(|a| id_of(a, &["activityId"]))
            .collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let stored = self.db.payloads_of("garmin_activities", &id_refs).await?;
        let stored_files = self.db.stored_activity_files().await?;

        let mut rows = Vec::with_capacity(listed.len());
        let mut to_fetch: Vec<(String, Value)> = Vec::new();
        for a in listed {
            let Some(id) = id_of(&a, &["activityId"]) else {
                continue;
            };
            let payload = a.to_string();
            let changed = stored.get(&id).is_none_or(|old| old != &payload);
            let wants_file = self.api.activity_files() && !stored_files.contains_key(&id);
            if changed || wants_file {
                to_fetch.push((id.clone(), a.clone()));
            }
            rows.push(ActivityRow {
                id_and_payload: WirePayload { id, payload },
                start_time_gmt: str_of(&a, &["startTimeGMT"]),
                activity_type: str_of(&a, &["activityType", "typeKey"]),
                name: str_of(&a, &["activityName"]),
            });
        }
        let keep: HashSet<&str> = rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
        upsert(self.db, &rows).await?;
        self.wrote(rows.len() as u64).await;

        self.progress.set_length(Some(to_fetch.len() as u64));
        let mut edges = CasEdgeAccumulator::new();
        for (id, listing) in &to_fetch {
            self.progress.set_message(&format!("garmin: activity {id}"));
            let changed = stored.get(id).is_none_or(|old| old != &listing.to_string());
            if changed {
                match self
                    .client
                    .get_json(&format!("/activity-service/activity/{id}"))
                    .await
                {
                    Ok(Fetched::Some(detail)) => {
                        upsert(
                            self.db,
                            &[ActivityDetailRow {
                                id_and_payload: WirePayload {
                                    id: id.clone(),
                                    payload: detail.to_string(),
                                },
                            }],
                        )
                        .await?;
                        s.activities_fetched += 1;
                    }
                    Ok(Fetched::Nothing) => {}
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(e) => {
                        s.errors += 1;
                        record_error(self.db, "garmin_activity_details", id, &format!("{e:#}"))
                            .await?;
                    }
                }
            }
            if self.api.activity_files() && !stored_files.contains_key(id) {
                match self
                    .client
                    .get_bytes(&format!("/download-service/files/activity/{id}"))
                    .await
                {
                    Ok(Fetched::Some(zip)) => match fit_from_zip(&zip) {
                        Ok(fit) => {
                            edges.add_fetched(
                                id,
                                FILE_KIND_FIT,
                                fit,
                                Some("application/vnd.ant.fit".into()),
                                Some(format!("{id}_ACTIVITY.fit")),
                            );
                            s.activity_files += 1;
                        }
                        Err(e) => {
                            s.errors += 1;
                            edges.add_failed(id, FILE_KIND_FIT, format!("{e:#}"));
                        }
                    },
                    Ok(Fetched::Nothing) => {}
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(e) => {
                        s.errors += 1;
                        edges.add_failed(id, FILE_KIND_FIT, format!("{e:#}"));
                    }
                }
            }
            self.progress.inc(1);
            if edges.bundle_mut().len() >= 20 {
                flush_activity_files(self.db, &edges).await?;
                edges = CasEdgeAccumulator::new();
                self.wrote(20).await;
            }
        }
        flush_activity_files(self.db, &edges).await?;

        // The listing is complete from `start`, so an activity dated
        // clearly inside the window that it did not name is gone
        // upstream. A day of slack keeps the local/GMT boundary out of it.
        let prune_from = format!("{} 00:00:00", ymd(start + Duration::days(1)));
        s.activities_pruned = self.db.prune_activities(&prune_from, &keep).await?;
        self.db
            .set_cursor(CURSOR_ACTIVITIES, &ymd(self.today))
            .await?;
        Ok(())
    }

    // ── wellness FIT bundles ─────────────────────────────────────────

    async fn wellness(&mut self, s: &mut FetchSummary) -> Result<()> {
        if !self.api.wellness_files() {
            return Ok(());
        }
        let start = self.resume_from(CURSOR_WELLNESS).await?;
        let stored = self.db.stored_wellness_files().await?;
        let mut day = start;
        let mut edges = CasEdgeAccumulator::new();
        while day <= self.today {
            let d = ymd(day);
            // Unlike the JSON metrics a day's bundle does not get
            // corrected after the fact, so one already stored is left
            // alone even inside the refresh window.
            if !stored.contains_key(&d) {
                self.progress.set_message(&format!("garmin: wellness {d}"));
                match self
                    .client
                    .get_bytes(&format!("/download-service/files/wellness/{d}"))
                    .await
                {
                    Ok(Fetched::Some(zip)) => {
                        edges.add_fetched(
                            &d,
                            FILE_KIND_WELLNESS_ZIP,
                            zip,
                            Some("application/zip".into()),
                            Some(format!("{d}_wellness.zip")),
                        );
                        s.wellness_files += 1;
                    }
                    Ok(Fetched::Nothing) => {}
                    Err(e) if is_auth(&e) => return Err(e),
                    Err(e) => {
                        s.errors += 1;
                        edges.add_failed(&d, FILE_KIND_WELLNESS_ZIP, format!("{e:#}"));
                    }
                }
                if edges.bundle_mut().len() >= 20 {
                    flush_wellness_files(self.db, &edges).await?;
                    edges = CasEdgeAccumulator::new();
                    self.wrote(20).await;
                }
            }
            self.db.set_cursor(CURSOR_WELLNESS, &d).await?;
            day += Duration::days(1);
        }
        flush_wellness_files(self.db, &edges).await?;
        Ok(())
    }

    // ── whole-account listings ───────────────────────────────────────

    async fn items(&mut self, s: &mut FetchSummary) -> Result<()> {
        for kind in ITEM_KINDS {
            let (path, id_keys): (String, &[&str]) = match *kind {
                "personal_records" => (
                    format!(
                        "/personalrecord-service/personalrecord/prs/{}",
                        self.account.display_name
                    ),
                    &["id"],
                ),
                "gear" => {
                    let Some(pk) = &self.account.profile_pk else {
                        warn!(
                            event = "garmin_gear_skipped",
                            "socialProfile carries no profileId"
                        );
                        continue;
                    };
                    (
                        format!("/gear-service/gear/filterGear?userProfilePk={pk}"),
                        &["gearPk", "uuid"],
                    )
                }
                "badges" => (
                    "/badge-service/badge/earned".to_string(),
                    &["badgeId", "badgeUuid"],
                ),
                "workouts" => (
                    "/workout-service/workouts?start=0&limit=1000".to_string(),
                    &["workoutId"],
                ),
                "goals" => (
                    "/goal-service/goal/goals?status=active&start=0&limit=1000&sortOrder=asc"
                        .to_string(),
                    &["id", "goalId"],
                ),
                _ => unreachable!(),
            };
            let list = match self.client.get_json(&path).await {
                Ok(Fetched::Some(Value::Array(a))) => a,
                Ok(Fetched::Some(other)) => {
                    // A wrapped listing: take the first array inside.
                    other
                        .as_object()
                        .and_then(|o| o.values().find_map(|v| v.as_array().cloned()))
                        .unwrap_or_default()
                }
                Ok(Fetched::Nothing) => Vec::new(),
                Err(e) if is_auth(&e) => return Err(e),
                Err(e) => {
                    s.errors += 1;
                    warn!(event = "garmin_items_failed", kind, error = %format!("{e:#}"));
                    continue;
                }
            };
            let mut rows = Vec::with_capacity(list.len());
            for item in &list {
                let Some(upstream_id) = id_of(item, id_keys) else {
                    warn!(
                        event = "garmin_item_without_id",
                        kind, "skipping an item with no id"
                    );
                    continue;
                };
                rows.push(ItemRow {
                    id_and_payload: WirePayload {
                        id: ItemRow::id_for(kind, &upstream_id),
                        payload: item.to_string(),
                    },
                    kind: kind.to_string(),
                    upstream_id,
                });
            }
            let keep: HashSet<&str> = rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
            upsert(self.db, &rows).await?;
            s.items += rows.len();
            s.items_pruned += self.db.prune_items(kind, &keep).await?;
            self.wrote(rows.len() as u64).await;
        }
        Ok(())
    }
}

async fn flush_activity_files(db: &RawDb, edges: &CasEdgeAccumulator) -> Result<()> {
    edges
        .flush(db.pool(), db.cas(), |activity_id, file_kind, blake3| {
            ActivityFileRow {
                id: format!("{activity_id}#{file_kind}"),
                activity_id: activity_id.to_string(),
                file_kind: file_kind.to_string(),
                blake3: blake3.map(str::to_string),
            }
        })
        .await
}

async fn flush_wellness_files(db: &RawDb, edges: &CasEdgeAccumulator) -> Result<()> {
    edges
        .flush(db.pool(), db.cas(), |calendar_date, file_kind, blake3| {
            WellnessFileRow {
                id: format!("{calendar_date}#{file_kind}"),
                calendar_date: calendar_date.to_string(),
                file_kind: file_kind.to_string(),
                blake3: blake3.map(str::to_string),
            }
        })
        .await
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
/// weigh-in.
pub fn weigh_in_rows(v: &Value) -> Vec<WeighInRow> {
    let mut out = Vec::new();
    let summaries = v["dailyWeightSummaries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
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
        let rows = weigh_in_rows(&v);
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

    #[test]
    fn empty_answers_are_recognised() {
        assert!(is_empty_answer(&json!(null)));
        assert!(is_empty_answer(&json!([])));
        assert!(is_empty_answer(&json!({})));
        assert!(!is_empty_answer(&json!({"a": 1})));
        assert!(!is_empty_answer(&json!(0)));
    }
}
