//! CalDAV download (RFC 4791): discover the account's calendars, then
//! keep each one in step with `sync-collection`, one resource per event
//! series. Fastmail, iCloud, Nextcloud and Google's CalDAV all answer it.

pub mod dav;

use std::collections::HashSet;

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::{self, RecordProblem, RunProblem};
use datalib_etl::http::LatchkeySettings;
use datalib_etl::progress::Progress;
use tracing::{info, warn};

use super::db::RawDb;
use super::schema_raw::{AccountRow, CalendarRow, IcsObjectRow};
use super::{select_calendars, FetchSummary};
use crate::ical;
use dav::{DavError, DavResponse, Multistatus};

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub server_url: String,
    /// Calendar names or ids to mirror; empty for all of them.
    pub calendars: Vec<String>,
    pub latchkey: LatchkeySettings,
    pub progress: Progress,
    pub control: DownloadControl,
}

/// How many resources one `calendar-multiget` names.
const MULTIGET_BATCH: usize = 100;

/// How many truncated `sync-collection` replies one calendar follows
/// before giving up on this run; the token taken so far is kept.
const MAX_SYNC_ROUNDS: usize = 50;

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = &opts.db;
    let mut summary = FetchSummary::default();
    let lk = &opts.latchkey;

    let found = discover(&opts.server_url, lk, &mut summary).await?;
    let account_id = dav::origin(&found.home_url)
        .and_then(|o| o.split("://").nth(1))
        .unwrap_or("caldav")
        .to_string();
    db.upsert_account(&AccountRow {
        id: account_id.clone(),
        method: "caldav".into(),
        server_url: Some(opts.server_url.clone()),
        principal_href: Some(found.principal_url.clone()),
        login: found.login.clone(),
    })
    .await?;
    info!(
        event = "caldav_discovery",
        principal = %found.principal_url,
        home = %found.home_url,
        "discovered the principal and its calendar home"
    );

    summary.requests += 1;
    let listing = dav::propfind(&found.home_url, "1", dav::BODY_LIST_CALENDARS, lk)
        .await
        .map_err(|e| anyhow::anyhow!("list calendars: {e}"))?;
    let calendars = calendars_in(&account_id, &found.home_url, &listing);
    db.upsert_calendars(&calendars.iter().map(|c| c.row.clone()).collect::<Vec<_>>())
        .await?;

    let selected = select_calendars(
        db,
        &opts.calendars,
        calendars
            .iter()
            .map(|c| (&c.row.id, c.row.display_name.as_deref())),
    )
    .await?;
    summary.calendars = selected.len();

    let mut run_problems: Vec<RunProblem> = Vec::new();
    let mut record_problems: Vec<RecordProblem> = Vec::new();
    for cal in calendars.iter().filter(|c| selected.contains(&c.row.id)) {
        if opts.control.stop.requested() {
            break;
        }
        let label = cal.row.display_name.as_deref().unwrap_or(&cal.row.id);
        opts.progress
            .set_message(&format!("syncing calendar {label}"));
        if let Err(e) = sync_calendar(db, cal, lk, &mut summary, &mut record_problems).await {
            summary.errors += 1;
            run_problems.push(RunProblem::listing(
                &format!("calendar {label}"),
                format!("{e:#}"),
            ));
        }
    }
    download_problems::report_run(db.pool(), &run_problems).await;
    download_problems::report_records(db.pool(), &record_problems).await;
    Ok(summary)
}

struct Discovered {
    principal_url: String,
    home_url: String,
    login: Option<String>,
}

/// `current-user-principal` from the configured URL, falling back to the
/// host's `/.well-known/caldav` (RFC 6764) when that URL does not answer
/// with one — Fastmail's bare host is a 404.
async fn discover(
    server_url: &str,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
) -> Result<Discovered> {
    let mut tried: Vec<String> = Vec::new();
    let mut candidates = vec![server_url.to_string()];
    if let Some(o) = dav::origin(server_url) {
        candidates.push(format!("{o}/.well-known/caldav"));
    }
    let mut principal = None;
    for url in candidates {
        summary.requests += 1;
        match dav::propfind(&url, "0", dav::BODY_CURRENT_USER_PRINCIPAL, lk).await {
            Ok(ms) => {
                if let Some(href) = ms
                    .responses
                    .iter()
                    .find_map(|r| r.current_user_principal.clone())
                {
                    principal = dav::absolutize(&url, &href);
                    break;
                }
                tried.push(format!("{url}: no current-user-principal"));
            }
            Err(e) => tried.push(format!("{url}: {e}")),
        }
    }
    let principal_url = principal.with_context(|| {
        format!(
            "no CalDAV principal found — tried {}. Check the server URL and that latchkey \
             holds a login for this host.",
            tried.join("; ")
        )
    })?;

    summary.requests += 1;
    let ms = dav::propfind(&principal_url, "0", dav::BODY_PRINCIPAL, lk)
        .await
        .map_err(|e| anyhow::anyhow!("propfind calendar-home-set: {e}"))?;
    let home = ms
        .responses
        .iter()
        .find_map(|r| r.calendar_home_set.clone())
        .context("the principal has no calendar-home-set")?;
    let home_url = dav::absolutize(&principal_url, &home).context("calendar home URL")?;
    let login = ms
        .responses
        .iter()
        .flat_map(|r| r.user_addresses.iter())
        .find_map(|a| {
            a.strip_prefix("mailto:")
                .or_else(|| a.strip_prefix("MAILTO:"))
        })
        .map(str::to_string)
        .or_else(|| last_segment(&principal_url));
    Ok(Discovered {
        principal_url,
        home_url,
        login,
    })
}

struct Calendar {
    row: CalendarRow,
    url: String,
}

/// The collections of a home listing that hold events.
fn calendars_in(account_id: &str, home_url: &str, listing: &Multistatus) -> Vec<Calendar> {
    listing
        .responses
        .iter()
        .filter(|r| r.is_calendar)
        .filter(|r| r.components.is_empty() || r.components.iter().any(|c| c == "VEVENT"))
        .filter_map(|r| {
            let id = last_segment(&r.href)?;
            Some(Calendar {
                url: dav::absolutize(home_url, &r.href)?,
                row: CalendarRow {
                    id,
                    account_id: account_id.to_string(),
                    href: Some(r.href.clone()),
                    display_name: r.display_name.clone(),
                    description: r.description.clone(),
                    color: r.color.clone(),
                    time_zone: r.calendar_timezone.as_deref().and_then(timezone_id),
                },
            })
        })
        .collect()
}

/// The TZID of a `calendar-timezone` value.
fn timezone_id(vcalendar: &str) -> Option<String> {
    ical::parse(vcalendar)
        .iter()
        .flat_map(|c| c.children_named("VTIMEZONE"))
        .find_map(|tz| tz.text("TZID"))
}

fn last_segment(href: &str) -> Option<String> {
    href.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && !s.contains("://"))
        .map(str::to_string)
}

async fn sync_calendar(
    db: &RawDb,
    cal: &Calendar,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
    problems: &mut Vec<RecordProblem>,
) -> Result<()> {
    let id = &cal.row.id;
    let mut token = db.sync_token(id).await?.unwrap_or_default();
    let full = token.is_empty();
    let mut listed: HashSet<String> = HashSet::new();
    for _ in 0..MAX_SYNC_ROUNDS {
        summary.requests += 1;
        let ms = match dav::report(&cal.url, "1", &dav::body_sync_collection(&token), lk).await {
            Ok(ms) => ms,
            // A token the server no longer honours: RFC 6578 answers 403
            // with `valid-sync-token`, some servers 409 or 410. Start over.
            Err(DavError::Http {
                status: 403 | 409 | 410,
                ..
            }) if !token.is_empty() => {
                warn!(event = "caldav_sync_token_refused", calendar = %id, "the server refused the stored sync token; listing the calendar whole");
                db.set_sync_token(id, None).await?;
                return Box::pin(sync_calendar(db, cal, lk, summary, problems)).await;
            }
            Err(DavError::Http {
                status: 403 | 405 | 501,
                ..
            }) => return list_whole(db, cal, lk, summary, problems).await,
            Err(e) => return Err(anyhow::anyhow!("sync-collection REPORT: {e}")),
        };
        let own = cal.url.trim_end_matches('/');
        let truncated = ms.responses.iter().any(|r| {
            r.status == Some(507)
                && dav::absolutize(&cal.url, &r.href)
                    .is_some_and(|u| u.trim_end_matches('/') == own)
        });
        let resources: Vec<DavResponse> = ms
            .responses
            .into_iter()
            .filter(|r| r.status != Some(507))
            .collect();
        listed.extend(
            resources
                .iter()
                .filter(|r| !matches!(r.status, Some(404 | 410)))
                .map(|r| r.href.clone()),
        );
        apply(db, cal, lk, resources, summary, problems).await?;
        let Some(next) = ms.sync_token else {
            anyhow::bail!("sync-collection reply carried no sync-token");
        };
        let moved = next != token;
        token = next;
        db.set_sync_token(id, Some(&token)).await?;
        if !truncated || !moved {
            break;
        }
    }
    if full {
        // A whole listing is the calendar as it is now: anything stored
        // that it did not name was deleted while no token was held.
        drop_unlisted(db, id, &listed, summary).await?;
    }
    Ok(())
}

/// For a server with no `sync-collection`: list every event, every run.
async fn list_whole(
    db: &RawDb,
    cal: &Calendar,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
    problems: &mut Vec<RecordProblem>,
) -> Result<()> {
    warn!(event = "caldav_sync_collection_unsupported", calendar = %cal.row.id, "the server does not support sync-collection; listing the calendar whole");
    summary.requests += 1;
    let ms = dav::report(&cal.url, "1", dav::BODY_QUERY_ALL_EVENTS, lk)
        .await
        .map_err(|e| anyhow::anyhow!("calendar-query REPORT: {e}"))?;
    let listed: HashSet<String> = ms.responses.iter().map(|r| r.href.clone()).collect();
    apply(db, cal, lk, ms.responses, summary, problems).await?;
    drop_unlisted(db, &cal.row.id, &listed, summary).await
}

async fn drop_unlisted(
    db: &RawDb,
    calendar_id: &str,
    listed: &HashSet<String>,
    summary: &mut FetchSummary,
) -> Result<()> {
    let gone: Vec<String> = db
        .ics_hrefs(calendar_id)
        .await?
        .into_iter()
        .filter(|(href, _)| !listed.contains(href))
        .map(|(_, uid)| uid)
        .collect();
    summary.events_deleted += gone.len();
    db.delete_ics_uids(calendar_id, &gone).await
}

/// Store what a listing changed and drop what it deleted. A resource
/// named without its data is fetched with `calendar-multiget`.
async fn apply(
    db: &RawDb,
    cal: &Calendar,
    lk: &LatchkeySettings,
    resources: Vec<DavResponse>,
    summary: &mut FetchSummary,
    problems: &mut Vec<RecordProblem>,
) -> Result<()> {
    let id = &cal.row.id;
    let stored = db.ics_hrefs(id).await?;
    let mut deleted: Vec<String> = Vec::new();
    let mut with_data: Vec<DavResponse> = Vec::new();
    let mut without_data: Vec<String> = Vec::new();
    for r in resources {
        if matches!(r.status, Some(404 | 410)) {
            deleted.extend(stored.get(&r.href).cloned());
        } else if r.href.trim_end_matches('/')
            == cal.row.href.as_deref().unwrap_or("").trim_end_matches('/')
        {
            // The collection itself, which some servers list first.
        } else if r.calendar_data.is_some() {
            with_data.push(r);
        } else {
            without_data.push(r.href);
        }
    }
    for chunk in without_data.chunks(MULTIGET_BATCH) {
        summary.requests += 1;
        let ms = dav::report(&cal.url, "1", &dav::body_multiget(chunk), lk)
            .await
            .map_err(|e| anyhow::anyhow!("calendar-multiget REPORT: {e}"))?;
        with_data.extend(
            ms.responses
                .into_iter()
                .filter(|r| r.calendar_data.is_some()),
        );
    }

    let mut rows: Vec<IcsObjectRow> = Vec::with_capacity(with_data.len());
    for r in &with_data {
        let data = r.calendar_data.as_deref().unwrap_or_default();
        let Some(uid) = ical::first_event_uid(data) else {
            summary.errors += 1;
            problems.push(RecordProblem::new(
                "ics_objects",
                &r.href,
                "the calendar object has no VEVENT with a UID, so it cannot be stored",
            ));
            continue;
        };
        match stored.get(&r.href) {
            Some(_) => summary.events_updated += 1,
            None => summary.events_new += 1,
        }
        rows.push(IcsObjectRow::new(
            id,
            &uid,
            Some(r.href.clone()),
            r.etag.clone(),
            data,
        ));
    }
    db.upsert_ics_objects(&rows).await?;
    summary.events_deleted += deleted.len();
    db.delete_ics_uids(id, &deleted).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_listing_keeps_only_event_calendars() {
        let listing = Multistatus {
            responses: vec![
                DavResponse {
                    href: "/dav/calendars/user/p/".into(),
                    ..Default::default()
                },
                DavResponse {
                    href: "/dav/calendars/user/p/bridge-uuid/".into(),
                    is_calendar: true,
                    display_name: Some("Bridge".into()),
                    components: vec!["VEVENT".into()],
                    ..Default::default()
                },
                DavResponse {
                    href: "/dav/calendars/user/p/tasks/".into(),
                    is_calendar: true,
                    components: vec!["VTODO".into()],
                    ..Default::default()
                },
                DavResponse {
                    href: "/dav/calendars/user/p/Inbox/".into(),
                    ..Default::default()
                },
            ],
            sync_token: None,
        };
        let cals = calendars_in(
            "caldav.test",
            "https://caldav.test/dav/calendars/user/p/",
            &listing,
        );
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].row.id, "bridge-uuid");
        assert_eq!(
            cals[0].url,
            "https://caldav.test/dav/calendars/user/p/bridge-uuid/"
        );
    }

    #[test]
    fn a_calendar_timezone_names_its_zone() {
        assert_eq!(
            timezone_id("BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Zurich\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n").as_deref(),
            Some("Europe/Zurich")
        );
    }
}
