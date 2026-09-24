//! Google Calendar download, through the Calendar API v3 (latchkey's
//! `google-calendar` service). Events are listed unexpanded
//! (`singleEvents=false`): a recurring event is one resource with its
//! rule, and each changed or cancelled occurrence is its own resource
//! naming the series in `recurringEventId`.

use std::collections::HashSet;

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::{self, RunProblem};
use datalib_etl::http::{
    default_retryability, latchkey_curl_classified, HttpRequest, HttpResponse, HttpService,
    LatchkeySettings, Retryability,
};
use datalib_etl::progress::Progress;
use serde_json::Value;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::{AccountRow, CalendarRow, GoogleEventRow};
use super::{select_calendars, FetchSummary};

pub const HTTP_SERVICE: HttpService = HttpService::GoogleCalendar;
pub const BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct FetchOptions {
    pub db: RawDb,
    /// Calendar names or ids to mirror; empty for every calendar on the
    /// account's list.
    pub calendars: Vec<String>,
    pub latchkey: LatchkeySettings,
    pub progress: Progress,
    pub control: DownloadControl,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = &opts.db;
    let lk = &opts.latchkey;
    let mut summary = FetchSummary::default();

    let list = list_calendars(lk, &mut summary).await?;
    let login = list
        .iter()
        .find(|c| c.get("primary").and_then(Value::as_bool) == Some(true))
        .and_then(|c| str_of(c, "id"));
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

    let selected = select_calendars(
        db,
        &opts.calendars,
        rows.iter().map(|c| (&c.id, c.display_name.as_deref())),
    )
    .await?;
    summary.calendars = selected.len();

    let mut problems: Vec<RunProblem> = Vec::new();
    for cal in rows.iter().filter(|c| selected.contains(&c.id)) {
        if opts.control.stop.requested() {
            break;
        }
        let label = cal.display_name.as_deref().unwrap_or(&cal.id);
        opts.progress
            .set_message(&format!("syncing calendar {label}"));
        if let Err(e) = sync_calendar(db, &cal.id, lk, &mut summary).await {
            summary.errors += 1;
            problems.push(RunProblem::listing(
                &format!("calendar {label}"),
                format!("{e:#}"),
            ));
        }
    }
    download_problems::report_run(db.pool(), &problems).await;
    Ok(summary)
}

async fn list_calendars(lk: &LatchkeySettings, summary: &mut FetchSummary) -> Result<Vec<Value>> {
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

pub fn calendar_list_url(page: Option<&str>) -> String {
    let mut url = format!("{BASE}/users/me/calendarList?maxResults=250&showHidden=true");
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", encode(p)));
    }
    url
}

fn calendar_row(c: &Value) -> Option<CalendarRow> {
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

/// One calendar: the changes since its sync token, or everything when
/// it has none. A full listing is the calendar as it is, so what it
/// does not name is dropped.
async fn sync_calendar(
    db: &RawDb,
    calendar_id: &str,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
) -> Result<()> {
    let token = db.sync_token(calendar_id).await?;
    let full = token.is_none();
    let known = db.google_event_ids(calendar_id).await?;
    let mut listed: HashSet<String> = HashSet::new();
    let mut page: Option<String> = None;
    let next_sync = loop {
        let url = events_url(calendar_id, token.as_deref(), page.as_deref());
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
                db.set_sync_token(calendar_id, None).await?;
                return Box::pin(sync_calendar(db, calendar_id, lk, summary)).await;
            }
            Err(e) => return Err(e),
        };
        let items = v
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        apply(db, calendar_id, &items, &known, &mut listed, summary).await?;
        page = str_of(&v, "nextPageToken");
        if page.is_none() {
            break str_of(&v, "nextSyncToken");
        }
    };
    if full {
        let gone: Vec<String> = known.difference(&listed).cloned().collect();
        summary.events_deleted += gone.len();
        db.delete_google_events(calendar_id, &gone).await?;
    }
    db.set_sync_token(calendar_id, next_sync.as_deref()).await
}

pub fn events_url(calendar_id: &str, sync_token: Option<&str>, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/calendars/{}/events?maxResults=2500&showDeleted=true&singleEvents=false",
        encode(calendar_id)
    );
    if let Some(t) = sync_token {
        url.push_str(&format!("&syncToken={}", encode(t)));
    }
    if let Some(p) = page {
        url.push_str(&format!("&pageToken={}", encode(p)));
    }
    url
}

/// Store a page of events. A cancelled occurrence is kept — it is how a
/// series says one of its dates is off — but a cancelled event or
/// series is a deletion, and takes its occurrences with it.
async fn apply(
    db: &RawDb,
    calendar_id: &str,
    items: &[Value],
    known: &HashSet<String>,
    listed: &mut HashSet<String>,
    summary: &mut FetchSummary,
) -> Result<()> {
    let mut rows = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for item in items {
        let Some(row) = GoogleEventRow::new(calendar_id, item) else {
            summary.errors += 1;
            continue;
        };
        let cancelled = row.status.as_deref() == Some("cancelled");
        if cancelled && row.recurring_event_id.is_none() {
            deleted.push(row.event_id.clone());
            continue;
        }
        listed.insert(row.event_id.clone());
        if known.contains(&row.event_id) {
            summary.events_updated += 1;
        } else {
            summary.events_new += 1;
        }
        rows.push(row);
    }
    db.upsert_google_events(&rows).await?;
    if !deleted.is_empty() {
        let mut gone = db.google_occurrences_of(calendar_id, &deleted).await?;
        gone.extend(deleted.into_iter().filter(|id| known.contains(id)));
        summary.events_deleted += gone.len();
        db.delete_google_events(calendar_id, &gone).await?;
    }
    Ok(())
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

/// Percent-encode a path segment or query value. Calendar ids carry
/// `@` and `#` (`en.usa#holiday@group.v.calendar.google.com`).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
