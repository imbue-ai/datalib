//! One stretch of time walked through `search.messages`, page by page,
//! newest first. Each page settles a span of the stretch the way a
//! history page does, so a search cut off part way leaves a smaller gap.
//! The reply search and the involves-me search both walk this way.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use datalib_etl_web::coverage::{self, Next, Span};
use datalib_etl_web::http::LatchkeySettings;

use super::shapes::M_SEARCH;
use super::{call, key_ts, settled};

/// Slack serves no page of a search past this one.
const MAX_PAGES: u64 = 100;

const PER_PAGE: &str = "100";

/// A `ts` key as the day it falls on, in UTC.
fn day_of(key: &str) -> NaiveDate {
    let secs: i64 = key_ts(key)
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .date_naive()
}

/// The `after:` and `before:` filters for `span`. They take whole days,
/// excluded, in the account's own time zone, so each reaches two days
/// past the span's ends.
pub(super) fn days_clause(span: &Span) -> String {
    let pad = chrono::Days::new(2);
    let after = day_of(&span.lo) - pad;
    let before = day_of(&span.hi) + pad;
    format!(
        "after:{} before:{}",
        after.format("%Y-%m-%d"),
        before.format("%Y-%m-%d")
    )
}

/// One page of a [`SearchWalk`]: every match it returned, and the span
/// of the stretch it settled.
pub(super) struct SearchPage {
    pub matches: Vec<Value>,
    pub covered: Option<Span>,
}

/// The pages of one stretch. `query` names the stretch it is asked for:
/// a walk that reaches Slack's last page before the bottom of the
/// stretch asks again below what it settled.
pub(super) struct SearchWalk<Q> {
    query: Q,
    stretch: Span,
    q: String,
    top: Option<String>,
    page: u64,
    done: bool,
}

impl<Q: Fn(&Span) -> String> SearchWalk<Q> {
    pub fn new(gap: &Span, query: Q) -> Self {
        let q = query(gap);
        Self {
            query,
            stretch: gap.clone(),
            q,
            top: Some(gap.hi.clone()),
            page: 1,
            done: false,
        }
    }

    /// The next page, or `None` when the stretch is settled. `counts`
    /// says which matches settle the stretch: a match outside what was
    /// searched for says nothing of it.
    pub async fn next(
        &mut self,
        counts: impl Fn(&Value) -> bool,
        latchkey: &LatchkeySettings,
        progress: &datalib_etl::progress::Progress,
        label: &str,
    ) -> Result<Option<SearchPage>> {
        if self.done {
            return Ok(None);
        }
        if self.page > MAX_PAGES {
            match coverage::next_below(&self.stretch, self.top.as_deref(), true) {
                Next::Below(below) => {
                    self.q = (self.query)(&below);
                    self.top = Some(below.hi.clone());
                    self.stretch = below;
                    self.page = 1;
                }
                Next::Done => {
                    self.done = true;
                    return Ok(None);
                }
                Next::Stuck => anyhow::bail!(
                    "{M_SEARCH}: {MAX_PAGES} pages settled nothing of {}..{}; the rest was not searched",
                    key_ts(&self.stretch.lo),
                    key_ts(&self.stretch.hi)
                ),
            }
        }
        progress.set_message(&format!("{M_SEARCH} {label} page {}", self.page));
        let mut params = BTreeMap::new();
        params.insert("query".to_string(), self.q.clone());
        params.insert("count".to_string(), PER_PAGE.to_string());
        params.insert("sort".to_string(), "timestamp".to_string());
        params.insert("sort_dir".to_string(), "desc".to_string());
        params.insert("page".to_string(), self.page.to_string());
        let resp = call(M_SEARCH, &params, latchkey).await?;
        let found = resp.get("messages");
        let matches: Vec<Value> = found
            .and_then(|m| m.get("matches"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pages = found
            .and_then(|m| m.get("paging"))
            .and_then(|p| p.get("pages"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let last = matches.is_empty() || self.page >= pages;
        let in_stretch: Vec<String> = matches
            .iter()
            .filter(|m| counts(m))
            .filter_map(|m| m.get("ts").and_then(Value::as_str).map(super::ts_key))
            .filter(|key| *key >= self.stretch.lo && *key <= self.stretch.hi)
            .collect();
        let covered = settled(&self.stretch, self.top.as_deref(), &in_stretch, last);
        if last {
            self.done = true;
        } else {
            if let Some(oldest) = in_stretch.into_iter().min() {
                self.top = Some(oldest);
            }
            self.page += 1;
        }
        Ok(Some(SearchPage { matches, covered }))
    }
}

/// The thread a match is in, read from its permalink: a match carries
/// no `thread_ts`. `None` for a message in no thread.
pub(super) fn permalink_thread_ts(m: &Value) -> Option<&str> {
    let permalink = m.get("permalink")?.as_str()?;
    let (_, query) = permalink.split_once('?')?;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("thread_ts="))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::ts_key;

    fn past_the_last_page(top: &str) -> SearchWalk<impl Fn(&Span) -> String> {
        let mut walk = SearchWalk::new(&Span::new("20", "90"), |s: &Span| {
            format!("{}..{}", s.lo, s.hi)
        });
        walk.top = Some(top.to_string());
        walk.page = MAX_PAGES + 1;
        walk
    }

    async fn next(walk: &mut SearchWalk<impl Fn(&Span) -> String>) -> Result<Option<SearchPage>> {
        let empty_tape = tempfile::tempdir().unwrap();
        let (latchkey, progress) = (
            LatchkeySettings::default(),
            datalib_etl::progress::Progress::noop(),
        );
        datalib_etl_web::playback::scope(
            empty_tape.path(),
            walk.next(|_| true, &latchkey, &progress, "test"),
        )
        .await
    }

    /// Slack serves no page past the hundredth. A walk that settled some
    /// of its stretch asks again below that; one that settled none of it
    /// says so rather than asking the same thing for good; one that
    /// reached the bottom is done.
    #[tokio::test]
    async fn past_the_last_page_a_walk_asks_again_below_what_it_settled() {
        let mut lowered = past_the_last_page("70");
        assert!(next(&mut lowered).await.is_err(), "the tape is empty");
        assert_eq!(lowered.stretch, Span::new("20", "70"));
        assert_eq!(lowered.q, "20..70");
        assert_eq!(lowered.top.as_deref(), Some("70"));
        assert_eq!(lowered.page, 1);

        let err = next(&mut past_the_last_page("90")).await.err().unwrap();
        assert!(format!("{err:#}").contains("settled nothing"), "{err:#}");

        let mut reached = past_the_last_page("20");
        assert!(next(&mut reached).await.unwrap().is_none());
        assert!(reached.done);
    }

    #[test]
    fn the_days_clause_pads_two_days_each_way() {
        let span = Span::new(ts_key("1790000000.000000"), ts_key("1790259200.000000"));
        assert_eq!(days_clause(&span), "after:2026-09-19 before:2026-09-26");
    }
}
