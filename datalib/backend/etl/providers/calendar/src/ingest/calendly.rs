//! Calendly download, through Calendly's API v2 (latchkey's `calendly`
//! service). The signed-in user's scheduled events are listed whole
//! every run, each page one transaction; the listing names every event,
//! so what a whole listing does not name is gone. An event's invitees
//! are a request of their own, owed whenever the event's listing entry
//! changes (`calendly_invitees`, held at the event's `listing_hash`)
//! and fetched through `datalib_etl_web::owed`. `INGEST.md` says which
//! of these shapes come from the published reference only.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpError, HttpRequest,
    HttpResponse, HttpService, LatchkeySettings, Retryability,
};
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Outcome};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::db::RawDb;
use super::schema_raw::{
    calendly_uuid, AccountRow, CalendarRow, CalendlyEventRow, CalendlyInviteesRow,
};
use super::{FetchSummary, Window};

pub const HTTP_SERVICE: HttpService = HttpService::Calendly;
pub const BASE: &str = "https://api.calendly.com";
/// The `accounts` row's id: one Calendly login per source.
pub const ACCOUNT_ID: &str = "calendly";
/// The most either listing returns per page.
const PAGE: usize = 100;
const INVITEES: &str = "calendly_invitees";

pub struct FetchOptions {
    pub db: RawDb,
    /// Only meetings that start in these days; `None` mirrors everything.
    pub window: Option<Window>,
    pub latchkey: LatchkeySettings,
    pub progress: Progress,
    pub control: DownloadControl,
    /// Seals as pages land, when the step driver hands one over.
    pub sealer: Option<Sealer>,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| sync(opts, found)).await
}

/// The signed-in user, as `/users/me` describes them.
#[derive(Debug, Clone)]
pub struct Me {
    pub uri: String,
    /// The last segment of `uri`: the calendar's id in the store.
    pub uuid: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub timezone: Option<String>,
    pub scheduling_url: Option<String>,
}

pub fn request(url: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url).header("Accept", "application/json")
}

pub fn me_url() -> String {
    format!("{BASE}/users/me")
}

/// One page of the user's scheduled events, soonest first, active and
/// canceled alike (no `status` filter).
pub fn events_url(user_uri: &str, window: Option<&Window>, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/scheduled_events?user={}&count={PAGE}&sort={}",
        percent_encode(user_uri),
        percent_encode("start_time:asc"),
    );
    if let Some(w) = window {
        if let Some(start) = w.start {
            url.push_str(&format!(
                "&min_start_time={}",
                percent_encode(&format!("{start}T00:00:00Z"))
            ));
        }
        if let Some(end) = w.end {
            url.push_str(&format!(
                "&max_start_time={}",
                percent_encode(&format!("{end}T00:00:00Z"))
            ));
        }
    }
    if let Some(p) = page {
        url.push_str(&format!("&page_token={}", percent_encode(p)));
    }
    url
}

/// One page of an event's invitees, canceled ones included, in the
/// order they booked.
pub fn invitees_url(event_uuid: &str, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/scheduled_events/{}/invitees?count={PAGE}&sort={}",
        percent_encode(event_uuid),
        percent_encode("created_at:asc"),
    );
    if let Some(p) = page {
        url.push_str(&format!("&page_token={}", percent_encode(p)));
    }
    url
}

async fn sync(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    let lk = &opts.latchkey;
    let stop = &opts.control.stop;
    let mut summary = FetchSummary::default();
    // Calendly has no `calendars` list to resolve; this clears what an
    // earlier config of this source may have left.
    found.config([]);

    opts.progress.set_message("reading the Calendly account");
    let me = me(lk, &mut summary).await?;
    db.upsert_account(&AccountRow {
        id: ACCOUNT_ID.into(),
        method: "calendly".into(),
        server_url: Some(BASE.into()),
        principal_href: Some(me.uri.clone()),
        login: me.email.clone(),
    })
    .await?;
    db.upsert_calendars(&[calendar_row(&me)]).await?;
    // A token for someone else: the other user's meetings go.
    summary.events_deleted += db
        .delete_calendars_not_in(ACCOUNT_ID, std::slice::from_ref(&me.uuid))
        .await?;
    summary.calendars = 1;

    opts.progress.set_message("listing Calendly meetings");
    let listing = Listing {
        db,
        lk,
        me: &me,
        window: opts.window.as_ref(),
        stop,
        sealer: opts.sealer.as_ref(),
    };
    match listing.run(&mut summary).await {
        Ok(None) => {}
        Ok(Some(held_back)) => found.listing("meetings", held_back),
        Err(e) => {
            summary.errors += 1;
            found.listing("meetings", format!("{e:#}"));
        }
    }
    if stop.requested() {
        return Ok(summary);
    }

    let listed = db.calendly_listed(&me.uuid).await?;
    let owed = owed::owed(db.pool(), INVITEES, listed).await?;
    opts.progress
        .set_message(&format!("reading the invitees of {} meetings", owed.len()));
    let fetcher = Invitees {
        lk,
        stop,
        requests: AtomicUsize::new(0),
    };
    let drained = owed::drain(
        &owed::Loop {
            pool: db.pool(),
            table: INVITEES,
            phase: "invitees",
            stop,
            found: &found,
            sealer: opts.sealer.as_ref(),
            batch: 1,
            concurrency: 1,
            flush: 50,
            flush_bytes: 0,
            failures_in_a_row: 10,
        },
        owed,
        &fetcher,
    )
    .await?;
    summary.requests += fetcher.requests.load(Ordering::Relaxed);
    summary.errors += drained.failed;
    Ok(summary)
}

pub async fn me(lk: &LatchkeySettings, summary: &mut FetchSummary) -> Result<Me> {
    summary.requests += 1;
    let v = get_json(&me_url(), lk)
        .await
        .context("read the Calendly user")?;
    let r = v
        .get("resource")
        .context("`/users/me` answered with no `resource`")?;
    let uri = str_of(r, "uri").context("`/users/me` named no user `uri`")?;
    let uuid = calendly_uuid(&uri)
        .with_context(|| format!("the user uri {uri:?} ends in no id"))?
        .to_string();
    Ok(Me {
        uuid,
        name: str_of(r, "name"),
        email: str_of(r, "email"),
        timezone: str_of(r, "timezone"),
        scheduling_url: str_of(r, "scheduling_url"),
        uri,
    })
}

pub fn calendar_row(me: &Me) -> CalendarRow {
    CalendarRow {
        id: me.uuid.clone(),
        account_id: ACCOUNT_ID.into(),
        href: Some(me.uri.clone()),
        display_name: Some("Calendly".into()),
        description: me.scheduling_url.clone(),
        color: None,
        time_zone: me.timezone.clone(),
    }
}

struct Listing<'a> {
    db: &'a RawDb,
    lk: &'a LatchkeySettings,
    me: &'a Me,
    window: Option<&'a Window>,
    stop: &'a StopFlag,
    sealer: Option<&'a Sealer>,
}

impl Listing<'_> {
    /// Every page of the user's events, each its own transaction. The
    /// last, when the listing named every event it listed, drops what
    /// it did not name; otherwise nothing is dropped and the returned
    /// reason says why. A stop ends it between pages, dropping nothing.
    async fn run(&self, summary: &mut FetchSummary) -> Result<Option<String>> {
        let calendar = self.me.uuid.as_str();
        let known = self.db.calendly_listing_hashes(calendar).await?;
        let mut listed: HashSet<String> = HashSet::new();
        let mut unidentified = 0usize;
        let mut page: Option<String> = None;
        loop {
            if self.stop.requested() {
                return Ok(None);
            }
            let url = events_url(&self.me.uri, self.window, page.as_deref());
            summary.requests += 1;
            let v = get_json(&url, self.lk).await?;
            // Without `collection` this is not a page, and reading it as
            // an empty one would drop every stored meeting.
            let items = v
                .get("collection")
                .and_then(Value::as_array)
                .context("the events reply carried no `collection` list")?;
            let next = v
                .pointer("/pagination/next_page_token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if next.is_some() && next == page {
                anyhow::bail!("the events listing handed back the page token it was given");
            }
            let mut rows = Vec::with_capacity(items.len());
            for item in items {
                let mut event = item.clone();
                order_sets(&mut event);
                match CalendlyEventRow::new(calendar, &event) {
                    Some(row) => rows.push(row),
                    None => {
                        summary.errors += 1;
                        unidentified += 1;
                    }
                }
            }
            for row in &rows {
                let key = &row.id_and_payload.id;
                match known.get(key) {
                    None => summary.events_new += 1,
                    Some(held) if *held != row.listing_hash => summary.events_updated += 1,
                    Some(_) => {}
                }
                listed.insert(key.clone());
            }
            let last = next.is_none();
            let mut tx = self.db.pool().begin().await.context("begin a page")?;
            RawDb::upsert_calendly_events_in_tx(&mut tx, &rows).await?;
            let mut wrote = rows.len() as u64;
            let held_back = (unidentified > 0).then(|| {
                format!(
                    "the listing named {unidentified} meeting(s) with no id, which could be any \
                     stored meeting, so none it did not name was dropped"
                )
            });
            if last && held_back.is_none() {
                let gone: Vec<String> = known
                    .keys()
                    .filter(|k| !listed.contains(*k))
                    .cloned()
                    .collect();
                summary.events_deleted += gone.len();
                wrote += gone.len() as u64;
                RawDb::delete_calendly_events(&mut tx, &gone).await?;
            }
            tx.commit().await.context("commit a page")?;
            if let Some(sealer) = self.sealer {
                sealer.wrote(wrote).await;
            }
            if last {
                return Ok(held_back);
            }
            page = next;
        }
    }
}

/// An event's lists of people are sets: put them in a fixed order, so an
/// event Calendly lists the same way twice stores the same payload and
/// the same `listing_hash`.
pub fn order_sets(event: &mut Value) {
    for (list, key) in [("event_memberships", "user"), ("event_guests", "email")] {
        if let Some(items) = event.get_mut(list).and_then(Value::as_array_mut) {
            items.sort_by_key(|item| sort_key(item, key));
        }
    }
}

fn sort_key(v: &Value, key: &str) -> (String, String) {
    (
        v.get(key).and_then(Value::as_str).unwrap_or("").to_string(),
        v.to_string(),
    )
}

/// One event's invitees, every page of them.
struct Invitees<'a> {
    lk: &'a LatchkeySettings,
    stop: &'a StopFlag,
    requests: AtomicUsize,
}

#[async_trait]
impl Fetcher<Vec<Value>> for Invitees<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Vec<Value>>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some((_, uuid)) = listed.key.rsplit_once('#') else {
                out.push(Fetched {
                    outcome: Outcome::Failed(format!("{}: not an event key", listed.key)),
                    listed,
                });
                continue;
            };
            let outcome = match self.list(uuid).await {
                Ok(invitees) => Outcome::Got(invitees),
                Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                Err(e) if ends_the_run(&e) => return Err(BatchError::Terminal(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Vec<Value>>],
    ) -> Result<()> {
        let rows: Vec<CalendlyInviteesRow> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(invitees) | Outcome::Unusable(invitees, ..) => {
                    Some(CalendlyInviteesRow::new(&f.listed.key, invitees))
                }
                _ => None,
            })
            .collect();
        RawDb::upsert_calendly_invitees_in_tx(tx, &rows).await
    }
}

impl Invitees<'_> {
    async fn list(&self, event_uuid: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            if self.stop.requested() {
                anyhow::bail!("stopped before the invitees were all read");
            }
            let url = invitees_url(event_uuid, page.as_deref());
            self.requests.fetch_add(1, Ordering::Relaxed);
            let v = get_json(&url, self.lk).await?;
            let items = v
                .get("collection")
                .and_then(Value::as_array)
                .context("the invitees reply carried no `collection` list")?;
            out.extend(items.iter().cloned());
            let next = v
                .pointer("/pagination/next_page_token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if next.is_none() {
                break;
            }
            if next == page {
                anyhow::bail!("the invitees listing handed back the page token it was given");
            }
            page = next;
        }
        // Calendly sorts by `created_at`; the uri breaks a tie the same
        // way every time.
        out.sort_by_key(|i| (str_of(i, "created_at"), str_of(i, "uri")));
        Ok(out)
    }
}

/// A refused credential, or the transport giving up: nothing after it
/// would fare better.
fn ends_the_run(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>().is_some()
        || matches!(
            e.downcast_ref::<HttpError>(),
            Some(HttpError::GaveUp { .. })
        )
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Refused(String);

async fn get_json(url: &str, lk: &LatchkeySettings) -> Result<Value> {
    let req = request(url).latchkey(lk.clone());
    let resp = latchkey_curl_classified(&req, retryability)
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status != 200 {
        return Err(api_error(url, &resp));
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parse JSON from {url}"))
}

/// Calendly says when a rate limit lifts in `X-RateLimit-Reset`, in
/// seconds, rather than `Retry-After`.
fn retryability(resp: &HttpResponse) -> Retryability {
    match default_retryability(resp) {
        Retryability::Retry { retry_after: None } if resp.status == 429 => Retryability::Retry {
            retry_after: resp
                .header("x-ratelimit-reset")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        },
        other => other,
    }
}

fn api_error(url: &str, resp: &HttpResponse) -> anyhow::Error {
    let body = resp.body_str();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            let title = str_of(&v, "title");
            let message = str_of(&v, "message");
            match (title, message) {
                (Some(t), Some(m)) => Some(format!("{t}: {m}")),
                (t, m) => t.or(m),
            }
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    match resp.status {
        401 | 403 => Refused(format!(
            "GET {url}: {}: {message}. Store a Calendly personal access token with \
             `latchkey auth set calendly -H \"Authorization: Bearer <token>\"`; it needs the \
             `users:read` and `scheduled_events:read` scopes.",
            resp.status
        ))
        .into(),
        s => anyhow::anyhow!("GET {url}: {s}: {message}"),
    }
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_listing_asks_for_one_user_soonest_first_in_the_window() {
        let window = Window {
            start: chrono::NaiveDate::from_ymd_opt(2026, 3, 1),
            end: chrono::NaiveDate::from_ymd_opt(2026, 4, 1),
        };
        let url = events_url(
            "https://api.calendly.com/users/PICARD",
            Some(&window),
            Some("tok+1"),
        );
        assert_eq!(
            url,
            "https://api.calendly.com/scheduled_events\
             ?user=https%3A%2F%2Fapi.calendly.com%2Fusers%2FPICARD&count=100\
             &sort=start_time%3Aasc&min_start_time=2026-03-01T00%3A00%3A00Z\
             &max_start_time=2026-04-01T00%3A00%3A00Z&page_token=tok%2B1"
        );
        assert_eq!(
            invitees_url("EV1", None),
            "https://api.calendly.com/scheduled_events/EV1/invitees?count=100&sort=created_at%3Aasc"
        );
    }

    #[test]
    fn a_rate_limit_waits_for_calendlys_reset() {
        let mut resp = HttpResponse {
            status: 429,
            headers: Default::default(),
            body: Vec::new(),
            duration_ms: 0,
        };
        resp.headers.insert("x-ratelimit-reset".into(), "42".into());
        assert!(matches!(
            retryability(&resp),
            Retryability::Retry { retry_after: Some(d) } if d == Duration::from_secs(42)
        ));
        resp.status = 404;
        assert!(matches!(retryability(&resp), Retryability::Accept));
    }

    #[test]
    fn a_refused_token_ends_the_run_and_says_how_to_store_one() {
        let resp = HttpResponse {
            status: 401,
            headers: Default::default(),
            body: br#"{"title":"Unauthenticated","message":"The access token is invalid"}"#
                .to_vec(),
            duration_ms: 0,
        };
        let e = api_error("https://api.calendly.com/users/me", &resp);
        assert!(ends_the_run(&e));
        let said = e.to_string();
        assert!(
            said.contains("Unauthenticated: The access token is invalid"),
            "{said}"
        );
        assert!(said.contains("latchkey auth set calendly"), "{said}");
    }

    #[test]
    fn the_people_on_an_event_are_put_in_one_order() {
        let mut a = json!({"event_guests": [{"email": "troi@enterprise.test"}, {"email": "data@enterprise.test"}],
                           "event_memberships": [{"user": "u2"}, {"user": "u1"}]});
        let mut b = json!({"event_guests": [{"email": "data@enterprise.test"}, {"email": "troi@enterprise.test"}],
                           "event_memberships": [{"user": "u1"}, {"user": "u2"}]});
        order_sets(&mut a);
        order_sets(&mut b);
        assert_eq!(a, b);
        assert_eq!(a["event_memberships"][0]["user"], "u1");
    }
}
