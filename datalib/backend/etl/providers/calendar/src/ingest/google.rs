//! Google Calendar download, through the Calendar API v3 (latchkey's
//! `google-calendar` service). Events are listed unexpanded
//! (`singleEvents=false`): a recurring event is one resource with its
//! rule, and each changed or cancelled occurrence is its own resource
//! naming the series in `recurringEventId`. The listing carries whole
//! events, so nothing is owed after it: each page is one transaction,
//! and the token moves with the prune that ends a listing.

use std::collections::HashSet;

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpRequest, HttpResponse,
    HttpService, LatchkeySettings, Retryability,
};
use serde_json::Value;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::{AccountRow, CalendarRow, GoogleEventRow};
use super::{select_calendars, FetchSummary, Window};

pub const HTTP_SERVICE: HttpService = HttpService::GoogleCalendar;
pub const BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct FetchOptions {
    pub db: RawDb,
    /// Calendar names or ids to mirror; empty for every calendar on the
    /// account's list.
    pub calendars: Vec<String>,
    /// Only what falls in these days; `None` mirrors everything.
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
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        sync_account(opts, found)
    })
    .await
}

async fn wrote(sealer: Option<&Sealer>, rows: u64) {
    if let Some(sealer) = sealer {
        sealer.wrote(rows).await;
    }
}

async fn sync_account(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    let lk = &opts.latchkey;
    let mut summary = FetchSummary::default();

    let list = list_calendars(lk, &mut summary).await?;
    let login = primary_id(&list);
    db.upsert_account(&AccountRow {
        id: "google".into(),
        method: "google".into(),
        server_url: Some(BASE.into()),
        principal_href: None,
        login,
    })
    .await?;
    let rows: Vec<CalendarRow> = list.iter().filter_map(calendar_row).collect();
    db.upsert_calendars(&rows).await?;
    let listed: Vec<String> = rows.iter().map(|c| c.id.clone()).collect();
    summary.events_deleted += db.delete_calendars_not_in("google", &listed).await?;

    let selected = select_calendars(
        &found,
        &opts.calendars,
        rows.iter().map(|c| (&c.id, c.display_name.as_deref())),
    )?;
    summary.calendars = selected.len();

    for cal in rows.iter().filter(|c| selected.contains(&c.id)) {
        if opts.control.stop.requested() {
            break;
        }
        let label = cal.display_name.as_deref().unwrap_or(&cal.id);
        opts.progress
            .set_message(&format!("syncing calendar {label}"));
        let listing = format!("calendar {label}");
        let cal_opts = CalendarOptions {
            db,
            window: opts.window.as_ref(),
            lk,
            sealer: opts.sealer.as_ref(),
        };
        match sync_calendar(&cal_opts, &cal.id, &mut summary).await {
            Ok(None) => {}
            Ok(Some(held_back)) => found.listing(&listing, held_back),
            Err(e) => {
                summary.errors += 1;
                found.listing(&listing, format!("{e:#}"));
            }
        }
    }
    Ok(summary)
}

pub(crate) async fn list_calendars(
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let url = calendar_list_url(page.as_deref());
        summary.requests += 1;
        let v = get_json(&url, lk).await.context("list calendars")?;
        out.extend(
            v.get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|c| c.get("deleted").and_then(Value::as_bool) != Some(true))
                .cloned(),
        );
        page = str_of(&v, "nextPageToken");
        if page.is_none() {
            return Ok(out);
        }
    }
}

/// The primary calendar's id, which is the account's address.
pub(crate) fn primary_id(list: &[Value]) -> Option<String> {
    list.iter()
        .find(|c| c.get("primary").and_then(Value::as_bool) == Some(true))
        .and_then(|c| str_of(c, "id"))
}

pub fn calendar_list_url(page: Option<&str>) -> String {
    let mut url = format!("{BASE}/users/me/calendarList?maxResults=250&showHidden=true");
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", percent_encode(p)));
    }
    url
}

pub(crate) fn calendar_row(c: &Value) -> Option<CalendarRow> {
    Some(CalendarRow {
        id: str_of(c, "id")?,
        account_id: "google".into(),
        href: None,
        display_name: str_of(c, "summaryOverride").or_else(|| str_of(c, "summary")),
        description: str_of(c, "description"),
        color: str_of(c, "backgroundColor"),
        time_zone: str_of(c, "timeZone"),
    })
}

struct CalendarOptions<'a> {
    db: &'a RawDb,
    window: Option<&'a Window>,
    lk: &'a LatchkeySettings,
    sealer: Option<&'a Sealer>,
}

/// One calendar: the changes since its sync token, or everything when
/// it has none. A full listing is the calendar as it is, so what it
/// does not name is dropped — unless it listed an event it could not
/// identify, which could be any stored one; then nothing is, and the
/// returned reason says why. Each page is one transaction; the token
/// moves with the prune, in the last.
async fn sync_calendar(
    o: &CalendarOptions<'_>,
    calendar_id: &str,
    summary: &mut FetchSummary,
) -> Result<Option<String>> {
    let (db, window, lk) = (o.db, o.window, o.lk);
    // Google refuses a sync token beside a time bound, so a windowed
    // calendar is listed whole every run, and keeps no token for a later
    // unwindowed run to resume from.
    let token = match window {
        Some(_) => None,
        None => db.sync_token(calendar_id).await?,
    };
    let full = token.is_none();
    let known = db.google_event_ids(calendar_id).await?;
    let mut seen = Seen::default();
    let mut page: Option<String> = None;
    let next_sync = loop {
        let url = match window {
            Some(w) => windowed_events_url(calendar_id, w, page.as_deref()),
            None => events_url(calendar_id, token.as_deref(), page.as_deref()),
        };
        summary.requests += 1;
        let v = match get_json(&url, lk).await {
            Ok(v) => v,
            // The token expired or Google dropped it: list again.
            Err(e) if token.is_some() && is_gone(&e) => {
                warn!(
                    event = "google_calendar_sync_token_expired",
                    calendar = %calendar_id,
                    "Google expired the sync token; listing the calendar whole"
                );
                let mut tx = db.pool().begin().await.context("begin")?;
                RawDb::set_sync_token(&mut tx, calendar_id, None).await?;
                tx.commit().await.context("commit")?;
                return Box::pin(sync_calendar(o, calendar_id, summary)).await;
            }
            Err(e) => return Err(e),
        };
        // Google sends `items` on every events reply, `[]` when there is
        // nothing (measured live); a reply without it is not a page.
        let items = v
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .context("the events reply carried no `items` list")?;
        page = str_of(&v, "nextPageToken");
        let last = page.is_none();
        let mut tx = db.pool().begin().await.context("begin a page")?;
        let wrote_rows = apply(&mut tx, calendar_id, &items, &known, &mut seen, summary).await?;
        let held_back = (full && seen.unidentified > 0).then(|| {
            format!(
                "the listing named {} event(s) with no id, which could be any stored event, \
                 so nothing it did not name was deleted",
                seen.unidentified
            )
        });
        if last {
            if full && held_back.is_none() {
                let gone: Vec<String> = known.difference(&seen.listed).cloned().collect();
                summary.events_deleted += gone.len();
                RawDb::delete_google_events(&mut tx, calendar_id, &gone).await?;
            }
            let next_sync = str_of(&v, "nextSyncToken").filter(|_| window.is_none());
            RawDb::set_sync_token(&mut tx, calendar_id, next_sync.as_deref()).await?;
        }
        tx.commit().await.context("commit a page")?;
        wrote(o.sealer, wrote_rows).await;
        if last {
            break held_back;
        }
    };
    Ok(next_sync)
}

/// The listing of one window: every event with some part inside it —
/// a series whose occurrences reach into it included — and the changed
/// occurrences that fall inside it.
pub fn windowed_events_url(calendar_id: &str, window: &Window, page: Option<&str>) -> String {
    let mut url = events_url(calendar_id, None, page);
    if let Some(start) = window.start {
        url.push_str(&format!(
            "&timeMin={}",
            percent_encode(&format!("{start}T00:00:00Z"))
        ));
    }
    if let Some(end) = window.end {
        url.push_str(&format!(
            "&timeMax={}",
            percent_encode(&format!("{end}T00:00:00Z"))
        ));
    }
    url
}

pub fn events_url(calendar_id: &str, sync_token: Option<&str>, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/calendars/{}/events?maxResults=2500&showDeleted=true&singleEvents=false",
        percent_encode(calendar_id)
    );
    if let Some(t) = sync_token {
        url.push_str(&format!("&syncToken={}", percent_encode(t)));
    }
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", percent_encode(p)));
    }
    url
}

/// Store a page of events in `tx`. A cancelled occurrence is kept — it
/// is how a series says one of its dates is off — but a cancelled event
/// or series is a deletion, and takes its occurrences with it. Pages
/// come in no particular order, so `cancelled` carries the deleted
/// series across them: an occurrence listed after its series' deletion
/// is not stored, and one stored before it is removed with it. Returns
/// how many rows the page wrote or removed.
async fn apply(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    calendar_id: &str,
    items: &[Value],
    known: &HashSet<String>,
    seen: &mut Seen,
    summary: &mut FetchSummary,
) -> Result<u64> {
    let mut deleted: Vec<String> = Vec::new();
    let mut kept = Vec::new();
    for item in items {
        let Some(row) = GoogleEventRow::new(calendar_id, item) else {
            summary.errors += 1;
            seen.unidentified += 1;
            continue;
        };
        if row.status.as_deref() == Some("cancelled") && row.recurring_event_id.is_none() {
            seen.cancelled.insert(row.event_id.clone());
            deleted.push(row.event_id.clone());
        } else {
            kept.push(row);
        }
    }
    let rows: Vec<GoogleEventRow> = kept
        .into_iter()
        .filter(|r| {
            !r.recurring_event_id
                .as_ref()
                .is_some_and(|series| seen.cancelled.contains(series))
        })
        .collect();
    for row in &rows {
        seen.listed.insert(row.event_id.clone());
        if known.contains(&row.event_id) {
            summary.events_updated += 1;
        } else {
            summary.events_new += 1;
        }
    }
    RawDb::upsert_google_events_in_tx(tx, &rows).await?;
    let mut wrote = rows.len() as u64;
    if !deleted.is_empty() {
        let mut gone = RawDb::google_occurrences_of(tx, calendar_id, &deleted).await?;
        gone.extend(deleted);
        for id in &gone {
            seen.listed.remove(id);
        }
        // Only what an earlier run stored is a deletion; the rest were
        // stored and removed within this one.
        summary.events_deleted += gone.iter().filter(|id| known.contains(*id)).count();
        wrote += gone.len() as u64;
        RawDb::delete_google_events(tx, calendar_id, &gone).await?;
    }
    Ok(wrote)
}

/// What one calendar's listing has shown so far, across its pages.
#[derive(Default)]
struct Seen {
    /// Every event stored by this listing.
    listed: HashSet<String>,
    /// Series the listing deleted.
    cancelled: HashSet<String>,
    /// Events listed without an id, which no stored row can be matched to.
    unidentified: usize,
}

async fn get_json(url: &str, lk: &LatchkeySettings) -> Result<Value> {
    let req = HttpRequest::get(HTTP_SERVICE, url)
        .header("Accept", "application/json")
        .latchkey(lk.clone());
    let resp = latchkey_curl_classified(&req, retryability)
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status != 200 {
        return Err(api_error(url, &resp));
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parse JSON from {url}"))
}

/// Google answers a per-user rate limit with a 403 whose body says so;
/// that one is worth waiting out. Every other 403 is about access.
fn retryability(resp: &HttpResponse) -> Retryability {
    let body = resp.body_str();
    if resp.status == 403
        && (body.contains("rateLimitExceeded") || body.contains("userRateLimitExceeded"))
    {
        return Retryability::Retry { retry_after: None };
    }
    default_retryability(resp)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Gone(String);

fn is_gone(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Gone>().is_some()
}

fn api_error(url: &str, resp: &HttpResponse) -> anyhow::Error {
    let body = resp.body_str();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    match resp.status {
        410 => Gone(format!("GET {url}: 410: {message}")).into(),
        401 | 403 => anyhow::anyhow!(
            "GET {url}: {}: {message}. Log in with `latchkey auth browser google-calendar` \
             and approve every scope it asks for.",
            resp.status
        ),
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

    #[test]
    fn a_calendar_id_is_encoded_into_the_path() {
        let url = events_url("en.usa#holiday@group.v.calendar.google.com", None, None);
        assert!(
            url.contains("/calendars/en.usa%23holiday%40group.v.calendar.google.com/events?"),
            "{url}"
        );
        assert!(url.contains("singleEvents=false"));
        let url = events_url("picard@enterprise.test", Some("tok+1"), Some("p2"));
        assert!(url.ends_with("&syncToken=tok%2B1&pageToken=p2"), "{url}");
    }

    #[test]
    fn a_403_is_retried_only_when_it_is_a_rate_limit() {
        let resp = |body: &str| HttpResponse {
            status: 403,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        };
        assert!(matches!(
            retryability(&resp(
                r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#
            )),
            Retryability::Retry { .. }
        ));
        assert!(matches!(
            retryability(&resp(
                r#"{"error":{"errors":[{"reason":"insufficientPermissions"}]}}"#
            )),
            Retryability::Accept
        ));
    }

    #[test]
    fn a_calendar_list_entry_prefers_the_users_own_name() {
        let row = calendar_row(&serde_json::json!({
            "id": "c1@group.calendar.google.com", "summary": "Shared", "summaryOverride": "Away team",
            "timeZone": "America/Los_Angeles"
        }))
        .unwrap();
        assert_eq!(row.display_name.as_deref(), Some("Away team"));
        assert_eq!(row.time_zone.as_deref(), Some("America/Los_Angeles"));
    }
}
