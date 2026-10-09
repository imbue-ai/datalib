//! The two producers. In search mode `POST /v1/search`, newest edit
//! first, lists the workspace; a result is the whole page object, so
//! the listing's rows are the content, and the stretch of edit times a
//! walk has read is a `coverage` span. In roots mode the walk is the
//! enumeration: every page under a root gets its `GET /v1/pages/{id}`
//! each run, and its children come from its stored body. Either way a
//! page object lands held at its `last_edited_time`, in one transaction
//! per page of results, and that stamp is what its body and its
//! comments are owed against.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use datalib_etl::doltlite_raw as dr;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::owed::{self, Held};
use serde_json::Value;

use super::db::PageUpsert;
use super::official::NotionOfficialError;
use super::schema_raw::PAGES;
use super::{run_over, Ctx, FetchSummary};

/// The `coverage` scope of the search walk. One workspace per source,
/// so one of them.
pub const SEARCH_SCOPE: &str = "search";

/// A span's ends are Notion stamps (`2026-09-01T00:00:00.000Z`), which
/// sort as text. These sort below and above every one of them.
const START_OF_TIME: &str = "";
const END_OF_TIME: &str = "~";

/// Where a search walk stops: the first result edited before this ends
/// it, and every result edited at or after it is listed. `None` walks
/// to the end of the workspace. The point is the bottom of the lowest
/// gap in what is held, since search cannot skip what is covered to
/// reach what is not; the refresh window lowers it so a page shared
/// late, whose edit time is already covered, is listed once more.
pub fn search_stops_at(held: &[Span], full: bool, refresh_floor: Option<&str>) -> Option<String> {
    if full {
        return None;
    }
    let gaps = coverage::gaps(&Span::new(START_OF_TIME, END_OF_TIME), held);
    let lowest = gaps.first()?;
    if lowest.lo == START_OF_TIME {
        return None;
    }
    let at = lowest.lo.as_str();
    Some(match refresh_floor {
        Some(floor) if floor < at => floor.to_string(),
        _ => at.to_string(),
    })
}

/// `days` before the newest edit the store has looked at, as a Notion
/// stamp; `None` when nothing is held or the stamp will not parse.
pub fn refresh_floor(held: &[Span], days: u32) -> Option<String> {
    let top = coverage::merged(held.to_vec()).pop()?;
    let hi = DateTime::parse_from_rfc3339(&top.hi).ok()?;
    let floor = hi
        .with_timezone(&Utc)
        .checked_sub_signed(Duration::days(days.into()))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    Some(floor.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// How one page of search results ended the walk, if it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// A result older than the stop was met: the stretch down to the
    /// stop is read whole.
    AtStop,
    /// The last page: the stretch down to the beginning is read whole.
    AtEnd,
    /// `max_pages`, or a page of results that would not come: only the
    /// results read are covered.
    CutShort,
}

/// What one page of search results listed.
#[derive(Debug, Default)]
struct ResultsPage {
    rows: Vec<PageUpsert>,
    /// The stamps of every result read, pages and containers alike.
    newest: Option<String>,
    oldest: Option<String>,
    end: Option<Ended>,
}

/// Reads one page of results: the page objects at or after `stop_at`,
/// stopping at the first older one, and at `cap` listed in all.
fn read_results(
    results: &[Value],
    stop_at: Option<&str>,
    listed_so_far: usize,
    cap: Option<usize>,
) -> ResultsPage {
    let mut page = ResultsPage::default();
    for r in results {
        let edited = r.get("last_edited_time").and_then(Value::as_str);
        if let Some(e) = edited {
            page.newest.get_or_insert_with(|| e.to_string());
        }
        if let (Some(e), Some(at)) = (edited, stop_at) {
            if e < at {
                page.end = Some(Ended::AtStop);
                return page;
            }
        }
        if let Some(e) = edited {
            page.oldest = Some(e.to_string());
        }
        if r.get("object").and_then(Value::as_str) != Some("page") {
            continue;
        }
        let Some(row) = PageUpsert::from_object(r) else {
            continue;
        };
        page.rows.push(row);
        if cap.is_some_and(|m| listed_so_far + page.rows.len() >= m) {
            page.end = Some(Ended::CutShort);
            return page;
        }
    }
    page
}

/// The span a walk that has read `[oldest, newest]` covers, given how
/// its last page ended.
fn covered(newest: &str, oldest: &str, end: Ended, stop_at: Option<&str>) -> Span {
    let lo = match (end, stop_at) {
        (Ended::AtStop, Some(at)) => at,
        (Ended::AtEnd, _) => START_OF_TIME,
        _ => oldest,
    };
    Span::new(lo, newest)
}

/// What storing a page of listed objects came to.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stored {
    pub new: usize,
    pub updated: usize,
    pub skipped: usize,
}

/// The rows of `page` not already held at their stamp, and the count
/// of each kind. A page held at the stamp it is listed at is not
/// written: its payload carries a signed cover URL that differs on
/// every fetch, and rewriting it would make an unchanged page differ
/// from itself.
fn not_yet_held(rows: Vec<PageUpsert>, held: &HashMap<String, Held>) -> (Vec<PageUpsert>, Stored) {
    let mut stored = Stored::default();
    let keep: Vec<PageUpsert> = rows
        .into_iter()
        .filter(|r| match held.get(&r.id) {
            Some(h) if h.satisfies(&r.last_edited_time) => {
                stored.skipped += 1;
                false
            }
            Some(h) if h.fetched => {
                stored.updated += 1;
                true
            }
            _ => {
                stored.new += 1;
                true
            }
        })
        .collect();
    (keep, stored)
}

impl Ctx<'_> {
    /// Stores the objects of `rows` not held at their stamp, with
    /// `span` covered, in one transaction.
    async fn store_listed(&self, rows: Vec<PageUpsert>, span: Option<Span>) -> Result<Stored> {
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        let held = owed::held_versions(self.db.pool(), PAGES, ids).await?;
        let (rows, stored) = not_yet_held(rows, &held);
        let mut tx = self.db.pool().begin().await?;
        self.db.store_pages(&mut tx, &rows).await?;
        if let Some(span) = span {
            coverage::cover(&mut tx, SEARCH_SCOPE, span).await?;
        }
        tx.commit().await?;
        self.wrote(rows.len() as u64).await;
        self.opts.progress.inc(rows.len() as u64);
        Ok(stored)
    }

    /// Walks the search newest-first down to where what is held begins,
    /// storing each page of results as it comes. Fails only when the
    /// first page of results does; a later page that fails ends the
    /// walk with what it read, as a `listing:search` row.
    pub async fn search_walk(&self, s: &mut FetchSummary) -> Result<()> {
        let held = coverage::held(self.db.pool(), SEARCH_SCOPE).await?;
        let floor = match self.opts.refresh_window_days {
            0 => None,
            days => refresh_floor(&held, days),
        };
        let stop_at = search_stops_at(&held, self.opts.full_sync, floor.as_deref());
        tracing::info!(
            event = "notion_search_pass",
            stops_at = stop_at.as_deref().unwrap_or("(the end of the workspace)"),
            "one pass of the search"
        );
        let mut cursor: Option<String> = None;
        let mut newest: Option<String> = None;
        let mut oldest: Option<String> = None;
        loop {
            let resp = match self.client.search(cursor.as_deref(), false).await {
                Ok(resp) => resp,
                Err(_) if self.stop().requested() => return Ok(()),
                Err(e) if e.ends_the_run() => return Err(run_over(e)),
                Err(e) if cursor.is_none() => return Err(anyhow::anyhow!("notion search: {e}")),
                Err(e) => {
                    self.found.listing(
                        "search",
                        format!(
                            "the search stopped after {} pages of the workspace, so older \
                             edits were not looked at: {e}",
                            s.listed
                        ),
                    );
                    return Ok(());
                }
            };
            let results = resp
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut page =
                read_results(&results, stop_at.as_deref(), s.listed, self.opts.max_pages);
            let has_more = resp
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next = resp
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(String::from);
            if page.end.is_none() && (!has_more || next.is_none()) {
                page.end = Some(Ended::AtEnd);
            }
            if newest.is_none() {
                newest = page.newest.clone();
            }
            if page.oldest.is_some() {
                oldest = page.oldest.clone();
            }
            let span = match (&newest, &oldest, page.end) {
                (Some(hi), Some(lo), Some(end)) => Some(covered(hi, lo, end, stop_at.as_deref())),
                (Some(hi), Some(lo), None) => Some(Span::new(lo.as_str(), hi.as_str())),
                _ => None,
            };
            s.listed += page.rows.len();
            let stored = self.store_listed(page.rows, span).await?;
            s.new_pages += stored.new;
            s.upd_pages += stored.updated;
            s.skipped_pages += stored.skipped;
            match page.end {
                Some(Ended::CutShort) => {
                    self.found.listing(
                        "search",
                        format!(
                            "the search stopped at max_pages after {} pages, so older edits \
                             were not looked at; the next run lists them",
                            s.listed
                        ),
                    );
                    return Ok(());
                }
                Some(_) => return Ok(()),
                None => cursor = next,
            }
        }
    }

    /// One round of the roots walk: `GET /v1/pages/{id}` for each page
    /// of `frontier`, stored held at its stamp. Returns the ids whose
    /// object came, in frontier order; their stored bodies name the next
    /// round's frontier.
    pub async fn roots_round(
        &self,
        frontier: &[String],
        s: &mut FetchSummary,
    ) -> Result<Vec<String>> {
        let mut listed: Vec<String> = Vec::new();
        for pid in frontier {
            if self.stop().requested() {
                break;
            }
            if self.opts.max_pages.is_some_and(|m| s.listed >= m) {
                self.found.listing(
                    "roots",
                    format!(
                        "the walk stopped at max_pages after {} pages, so the rest of the \
                         tree was not looked at",
                        s.listed
                    ),
                );
                break;
            }
            self.opts.progress.set_message(pid);
            match self.client.get_page(pid).await {
                Ok(obj) => {
                    let Some(row) = PageUpsert::from_object(&obj) else {
                        continue;
                    };
                    s.listed += 1;
                    let stored = self.store_listed(vec![row], None).await?;
                    s.new_pages += stored.new;
                    s.upd_pages += stored.updated;
                    s.skipped_pages += stored.skipped;
                    listed.push(pid.clone());
                }
                Err(_) if self.stop().requested() => break,
                Err(e) if e.ends_the_run() => return Err(run_over(e)),
                // Deleted, or never shared with this credential: not
                // listed, and not failed.
                Err(e @ (NotionOfficialError::NotFound(_) | NotionOfficialError::Forbidden(_))) => {
                    self.unreadable.lock().unwrap().insert(pid.clone(), e);
                }
                Err(e) => {
                    let mut tx = self.db.pool().begin().await?;
                    dr::record_object_error(&mut tx, PAGES, pid, &e.to_string()).await?;
                    tx.commit().await?;
                    s.failed_pages += 1;
                }
            }
        }
        Ok(listed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(lo: &str, hi: &str) -> Span {
        Span::new(lo, hi)
    }

    const T1: &str = "2026-09-01T00:00:00.000Z";
    const T2: &str = "2026-09-02T00:00:00.000Z";
    const T3: &str = "2026-09-03T00:00:00.000Z";

    /// A first sync, and one whose first walk never reached the end,
    /// walk to the end; a complete one stops where what is held begins.
    #[test]
    fn the_walk_stops_at_the_bottom_of_the_lowest_gap() {
        assert_eq!(search_stops_at(&[], false, None), None);
        assert_eq!(search_stops_at(&[s(T2, T3)], false, None), None);
        assert_eq!(search_stops_at(&[s("", T2)], false, None), Some(T2.into()));
        assert_eq!(
            search_stops_at(&[s("", T1), s(T2, T3)], false, None),
            Some(T1.into()),
            "a walk cut short above left a gap below it"
        );
        assert_eq!(search_stops_at(&[s("", T2)], true, None), None);
    }

    /// The refresh window reaches below what is held, never above it.
    #[test]
    fn the_refresh_window_lowers_the_stop() {
        assert_eq!(
            search_stops_at(&[s("", T2)], false, Some(T1)),
            Some(T1.into())
        );
        assert_eq!(
            search_stops_at(&[s("", T2)], false, Some(T3)),
            Some(T2.into())
        );
        assert_eq!(
            refresh_floor(&[s("", T3)], 2).as_deref(),
            Some(T1),
            "in Notion's own spelling"
        );
        assert_eq!(refresh_floor(&[], 2), None);
        // A window longer than the calendar reaches panicked in the
        // subtraction; its floor is below every stamp.
        let floor = refresh_floor(&[s("", T3)], u32::MAX).unwrap();
        assert!(floor.as_str() < T1, "{floor}");
    }

    fn page(id: &str, edited: &str) -> Value {
        serde_json::json!({"object": "page", "id": id, "last_edited_time": edited})
    }

    /// A result at the stop is listed; the first one below it ends the
    /// page. Notion stamps to the minute, so an equal stamp is a page
    /// edited in the same minute as the newest one the last run saw.
    #[test]
    fn a_result_at_the_stop_is_listed_and_one_below_it_ends_the_walk() {
        let results = [page("a", T3), page("b", T2), page("c", T1)];
        let read = read_results(&results, Some(T2), 0, None);
        let ids: Vec<&str> = read.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(read.end, Some(Ended::AtStop));
        assert_eq!(
            covered(T3, T2, Ended::AtStop, Some(T2)),
            s(T2, T3),
            "down to the stop, which was already held"
        );
    }

    /// A container in the results is read for its stamp and not listed.
    #[test]
    fn containers_are_passed_over() {
        let ds = serde_json::json!({"object": "data_source", "id": "d", "last_edited_time": T3});
        let read = read_results(&[ds, page("a", T2)], None, 0, None);
        assert_eq!(read.rows.len(), 1);
        assert_eq!(read.newest.as_deref(), Some(T3));
        assert_eq!(read.oldest.as_deref(), Some(T2));
    }

    /// `max_pages` bounds what one run lists, and the span covers only
    /// what it read.
    #[test]
    fn max_pages_cuts_the_page_and_covers_what_was_read() {
        let results = [page("a", T3), page("b", T2), page("c", T1)];
        let read = read_results(&results, None, 0, Some(2));
        assert_eq!(read.rows.len(), 2);
        assert_eq!(read.end, Some(Ended::CutShort));
        assert_eq!(covered(T3, T2, Ended::CutShort, None), s(T2, T3));
        assert_eq!(covered(T3, T1, Ended::AtEnd, None), s("", T3));
    }

    #[test]
    fn a_page_held_at_its_stamp_is_not_written_again() {
        let rows = vec![
            PageUpsert::from_object(&page("a", T3)).unwrap(),
            PageUpsert::from_object(&page("b", T2)).unwrap(),
            PageUpsert::from_object(&page("c", T1)).unwrap(),
        ];
        let held: HashMap<String, Held> = [
            (
                "a".to_string(),
                Held {
                    fetched: true,
                    version: Some(T3.into()),
                },
            ),
            (
                "b".to_string(),
                Held {
                    fetched: true,
                    version: Some(T1.into()),
                },
            ),
        ]
        .into();
        let (keep, stored) = not_yet_held(rows, &held);
        let ids: Vec<&str> = keep.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
        assert_eq!((stored.new, stored.updated, stored.skipped), (1, 1, 1));
    }
}
