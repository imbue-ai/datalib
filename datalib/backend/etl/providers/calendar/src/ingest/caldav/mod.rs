//! CalDAV download (RFC 4791): discover the account's calendars, then
//! keep each one in step through `datalib_etl_web::dav::sync`, which lists,
//! stores and prunes, one resource per event series; this crate says
//! how an object becomes a row. Fastmail, iCloud, Nextcloud and Google's
//! CalDAV all answer `sync-collection`.

pub mod dav;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::dav::sync::{self, CollectionSync, ObjectStore};
use datalib_etl_web::http::LatchkeySettings;
use sqlx::{Sqlite, Transaction};
use tracing::info;

use super::db::RawDb;
use super::schema_raw::{AccountRow, CalendarRow, IcsObjectRow};
use super::{select_calendars, FetchSummary, Window};
use crate::ical;
use dav::{CalendarProps, Multistatus};

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub server_url: String,
    /// Calendar names or ids to mirror; empty for all of them.
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
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |problems| {
        sync_account(opts, problems)
    })
    .await
}

async fn sync_account(opts: FetchOptions, problems: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    let mut summary = FetchSummary::default();
    let lk = &opts.latchkey;

    let Reached {
        found,
        account_id,
        calendars,
    } = reach(&opts.server_url, lk, &mut summary).await?;
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

    db.upsert_calendars(&calendars.iter().map(|c| c.row.clone()).collect::<Vec<_>>())
        .await?;
    let listed: Vec<String> = calendars.iter().map(|c| c.row.id.clone()).collect();
    summary.events_deleted += db.delete_calendars_not_in(&account_id, &listed).await?;

    let selected = select_calendars(
        &problems,
        &opts.calendars,
        calendars
            .iter()
            .map(|c| (&c.row.id, c.row.display_name.as_deref())),
    )?;
    summary.calendars = selected.len();

    let run = sync::Run {
        pool: db.pool(),
        stop: &opts.control.stop,
        found: &problems,
        sealer: opts.sealer.as_ref(),
    };
    for cal in calendars.iter().filter(|c| selected.contains(&c.row.id)) {
        if opts.control.stop.requested() {
            break;
        }
        let id = &cal.row.id;
        let label = cal.row.display_name.as_deref().unwrap_or(id);
        opts.progress
            .set_message(&format!("syncing calendar {label}"));
        let listing = match &opts.window {
            Some(w) => CollectionSync::query(&dav::KIND, &cal.url, dav::body_query_window(w), lk),
            None => CollectionSync::new(&dav::KIND, &cal.url, db.sync_token(id).await?, lk),
        };
        let listing_name = format!("calendar {label}");
        match sync::sync_collection(&run, db, id, label, listing).await {
            Ok(synced) => {
                summary.events_new += synced.new;
                summary.events_updated += synced.updated;
                summary.events_deleted += synced.deleted;
                summary.errors += synced.unusable + synced.failed;
                summary.requests += synced.requests;
                if let Some(cut_short) = synced.cut_short {
                    problems.listing(
                        &listing_name,
                        format!(
                            "{cut_short}; nothing it has not reached is deleted until it finishes"
                        ),
                    );
                }
            }
            Err(e) => {
                summary.errors += 1;
                problems.listing(&listing_name, format!("{e:#}"));
            }
        }
    }
    Ok(summary)
}

#[async_trait]
impl ObjectStore for RawDb {
    type Props = CalendarProps;
    type Row = IcsObjectRow;

    fn row(
        &self,
        collection: &str,
        href: &str,
        etag: Option<&str>,
        data: &str,
    ) -> std::result::Result<IcsObjectRow, String> {
        let uid = ical::first_event_uid(data).ok_or_else(|| {
            "the calendar object has no VEVENT with a UID, so it cannot be stored".to_string()
        })?;
        Ok(IcsObjectRow::new(
            collection,
            &uid,
            Some(href.to_string()),
            etag.map(String::from),
            data,
        ))
    }

    async fn put(&self, tx: &mut Transaction<'_, Sqlite>, rows: &[&IcsObjectRow]) -> Result<()> {
        RawDb::upsert_ics_objects_in_tx(tx, rows).await
    }

    async fn remove(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        href: &str,
    ) -> Result<u64> {
        RawDb::delete_ics_href(tx, collection, href).await
    }

    async fn set_token(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        token: Option<&str>,
    ) -> Result<()> {
        RawDb::set_sync_token(tx, collection, token).await
    }
}

/// What discovery and the calendar listing found: everything a run
/// needs before it syncs, and all a probe reports.
pub(crate) struct Reached {
    pub(crate) found: Discovered,
    pub(crate) account_id: String,
    pub(crate) calendars: Vec<Calendar>,
}

pub(crate) async fn reach(
    server_url: &str,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
) -> Result<Reached> {
    let found = discover(server_url, lk, summary).await?;
    let account_id = dav::origin(&found.home_url)
        .and_then(|o| o.split("://").nth(1))
        .unwrap_or("caldav")
        .to_string();
    summary.requests += 1;
    let listing = dav::propfind(&found.home_url, "1", dav::BODY_LIST_CALENDARS, lk)
        .await
        .map_err(|e| anyhow::anyhow!("list calendars: {e}"))?;
    let calendars = calendars_in(&account_id, &found.home_url, &listing);
    Ok(Reached {
        found,
        account_id,
        calendars,
    })
}

pub(crate) struct Discovered {
    principal_url: String,
    home_url: String,
    pub(crate) login: Option<String>,
}

async fn discover(
    server_url: &str,
    lk: &LatchkeySettings,
    summary: &mut FetchSummary,
) -> Result<Discovered> {
    let principal_url = datalib_etl_web::dav::find_principal(
        dav::HTTP_SERVICE,
        server_url,
        "caldav",
        lk,
        &mut summary.requests,
    )
    .await?;

    summary.requests += 1;
    let ms = dav::propfind(&principal_url, "0", dav::BODY_PRINCIPAL, lk)
        .await
        .map_err(|e| anyhow::anyhow!("propfind calendar-home-set: {e}"))?;
    let home = ms
        .responses
        .iter()
        .find_map(|r| r.props.calendar_home_set.clone())
        .context("the principal has no calendar-home-set")?;
    let home_url = dav::absolutize(&principal_url, &home).context("calendar home URL")?;
    let login = ms
        .responses
        .iter()
        .flat_map(|r| r.props.user_addresses.iter())
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

pub(crate) struct Calendar {
    pub(crate) row: CalendarRow,
    url: String,
}

/// The collections of a home listing that hold events.
fn calendars_in(account_id: &str, home_url: &str, listing: &Multistatus) -> Vec<Calendar> {
    listing
        .responses
        .iter()
        .filter(|r| r.props.is_calendar)
        .filter(|r| {
            r.props.components.is_empty() || r.props.components.iter().any(|c| c == "VEVENT")
        })
        .filter_map(|r| {
            let id = last_segment(&r.href)?;
            Some(Calendar {
                url: dav::absolutize(home_url, &r.href)?,
                row: CalendarRow {
                    id,
                    account_id: account_id.to_string(),
                    href: Some(r.href.clone()),
                    display_name: r.props.display_name.clone(),
                    description: r.props.description.clone(),
                    color: r.props.color.clone(),
                    time_zone: r.props.calendar_timezone.as_deref().and_then(timezone_id),
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

#[cfg(test)]
mod tests {
    use super::dav::{CalendarProps, DavResponse};
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
                    props: CalendarProps {
                        is_calendar: true,
                        display_name: Some("Bridge".into()),
                        components: vec!["VEVENT".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
                DavResponse {
                    href: "/dav/calendars/user/p/tasks/".into(),
                    props: CalendarProps {
                        is_calendar: true,
                        components: vec!["VTODO".into()],
                        ..Default::default()
                    },
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
