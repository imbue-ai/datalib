//! The three ways a calendar reaches the raw store: CalDAV, Google's
//! API, and `.ics` files. Each calendar keeps its own sync position, so
//! the `calendars` filter can widen without a cursor swallowing it: a
//! calendar added to the list has no token yet and is listed whole.

pub mod caldav;
pub mod db;
pub mod google;
pub mod ics_dir;
pub mod schema_raw;

use std::collections::HashSet;

use anyhow::Result;
use datalib_etl::download_problems;

pub use db::{db_path_for, RawDb};

/// The days a windowed source mirrors, as instants: from midnight UTC
/// on `since` to midnight UTC after `until`, either end open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Window {
    pub start: Option<chrono::NaiveDate>,
    /// The day *after* `until`: the end is exclusive.
    pub end: Option<chrono::NaiveDate>,
}

impl Window {
    /// `None` when the config sets no window.
    pub fn from_config(w: datalib_etl_calendar_config::Window<'_>) -> Result<Option<Self>> {
        if w.is_open() {
            return Ok(None);
        }
        let day = |s: &str| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("date {s:?}: {e}"))
        };
        Ok(Some(Self {
            start: w.since.map(day).transpose()?,
            end: w
                .until
                .map(day)
                .transpose()?
                .map(|d| d + chrono::Duration::days(1)),
        }))
    }
}

/// What one run did: the step's summary line, and the run's `sync_runs`
/// record.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FetchSummary {
    pub calendars: usize,
    pub events_new: usize,
    pub events_updated: usize,
    pub events_deleted: usize,
    /// `.ics` files whose contents had not moved since the last run.
    pub files_skipped: usize,
    pub errors: usize,
    pub requests: usize,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "calendars={} new={} updated={} deleted={} files_skipped={} errors={} requests={}",
            self.calendars,
            self.events_new,
            self.events_updated,
            self.events_deleted,
            self.files_skipped,
            self.errors,
            self.requests,
        )
    }
}

/// The ids of the calendars to sync: every one when `configured` is
/// empty, else each entry matched by id or by name. An entry that
/// matches nothing is reported and costs only itself — unless nothing
/// matches at all, which fails the run rather than falling back to
/// every calendar the filter was there to exclude.
pub async fn select_calendars<'a>(
    db: &RawDb,
    configured: &[String],
    available: impl Iterator<Item = (&'a String, Option<&'a str>)>,
) -> Result<HashSet<String>> {
    let available: Vec<(&String, Option<&str>)> = available.collect();
    if configured.is_empty() {
        download_problems::report(db.pool(), &[]).await;
        return Ok(available.iter().map(|(id, _)| (*id).clone()).collect());
    }
    let names = || {
        available
            .iter()
            .map(|(id, name)| match name {
                Some(n) => format!("{n} ({id})"),
                None => (*id).clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let resolution = download_problems::resolve_configured("calendars", configured, |want| {
        available
            .iter()
            .find(|(id, name)| {
                id.as_str() == want || name.is_some_and(|n| n.eq_ignore_ascii_case(want))
            })
            .map(|(id, _)| (*id).clone())
            .ok_or_else(|| {
                format!(
                    "no calendar has that name or id; the account has {}",
                    names()
                )
            })
    });
    download_problems::report(db.pool(), &resolution.problems).await;
    if resolution.nothing_resolved() {
        anyhow::bail!(
            "none of the configured calendars ({}) exists; the account has {}",
            configured.join(", "),
            names()
        );
    }
    Ok(resolution.resolved.into_iter().collect())
}
