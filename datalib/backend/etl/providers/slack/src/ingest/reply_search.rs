//! New replies on threads whose roots history will not read again.
//!
//! A thread is owed when its stored root lists a newer `latest_reply`
//! than the thread is held at, and a root is only read again when its
//! stretch of history is: the refresh window, or a reset. A reply to an
//! older root changes nothing history would list. `search.messages`
//! finds it: each conversation's reply time is a range, the stretches
//! searched are `coverage` spans (`replies:<channel>`), and each run
//! searches the gaps. A reply newer than its stored root's `latest_reply`
//! sends the root to be read again, and the thread is then owed like any
//! other. INGEST.md § "Threads" has the rules.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use serde_json::Value;
use tracing::info;

use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;

use super::db::{self, MessageInput, RawDb};
use super::schema_raw::slack_thread_key;
use super::shapes::{M_HISTORY, M_SEARCH};
use super::{call, history_message_input, key_ts, settled, ts_key};

/// How far behind the run's now the searched range stops: a reply is
/// searchable a little after it is posted, and one the index has not
/// reached yet would otherwise sit inside a span already settled.
pub const SEARCH_LAG: ChronoDuration = ChronoDuration::minutes(30);

/// Conversations named in one query. Slack ORs `in:` filters.
const CONVERSATIONS_PER_QUERY: usize = 20;

/// Slack serves no page of a search past this one.
const MAX_PAGES: u64 = 100;

const PER_PAGE: &str = "100";

/// One conversation the run mirrors, as the plan sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Conversation {
    pub id: String,
    /// Some stretch of its history is covered. With none, every root it
    /// has is read fresh by the walk ahead, replies and all.
    pub history_held: bool,
    pub replies_held: Vec<Span>,
}

/// What [`plan`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Conversations whose whole reply range is settled without a search.
    pub seed: Vec<String>,
    /// One stretch of reply time and the conversations missing it,
    /// newest stretch first.
    pub searches: Vec<(Span, Vec<String>)>,
}

/// Which stretches of `[since, top]` each conversation still owes a
/// search of. Conversations owing the same stretch share its queries.
pub(super) fn plan(since: &str, top: &str, conversations: &[Conversation]) -> Plan {
    let mut out = Plan::default();
    if top <= since {
        return out;
    }
    let wanted = Span::new(since, top);
    let mut by_gap: BTreeMap<Span, Vec<String>> = BTreeMap::new();
    for c in conversations {
        if !c.history_held {
            out.seed.push(c.id.clone());
            continue;
        }
        for gap in coverage::gaps(&wanted, &c.replies_held) {
            by_gap.entry(gap).or_default().push(c.id.clone());
        }
    }
    out.searches = by_gap.into_iter().rev().collect();
    out
}

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

/// The query for the threads of `channels` with a message in `span`.
/// `after:` and `before:` take whole days, excluded, in the account's
/// own time zone, so each reaches two days past the span's ends.
pub(super) fn query(channels: &[String], span: &Span) -> String {
    let pad = chrono::Days::new(2);
    let after = day_of(&span.lo) - pad;
    let before = day_of(&span.hi) + pad;
    let mut q = String::from("is:thread");
    for c in channels {
        q.push_str(&format!(" in:<#{c}>"));
    }
    q.push_str(&format!(
        " after:{} before:{}",
        after.format("%Y-%m-%d"),
        before.format("%Y-%m-%d")
    ));
    q
}

/// A reply a search returned. A match carries no `thread_ts`; its
/// permalink names the thread it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplyHit {
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: String,
}

pub(super) fn reply_hit(m: &Value) -> Option<ReplyHit> {
    let ts = m.get("ts")?.as_str()?;
    let channel_id = m.get("channel")?.get("id")?.as_str()?;
    let permalink = m.get("permalink")?.as_str()?;
    let (_, query) = permalink.split_once('?')?;
    let thread_ts = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("thread_ts="))?;
    (thread_ts != ts).then(|| ReplyHit {
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts: thread_ts.to_string(),
    })
}

/// Roots read again, and the replies that sent them.
#[derive(Debug, Default)]
pub struct ReplySearchTotals {
    pub pages: u64,
    pub replies_found: usize,
    pub roots_reread: usize,
}

pub(super) struct ReplySearch<'a> {
    pub db: &'a RawDb,
    pub team_id: &'a str,
    pub latchkey: &'a LatchkeySettings,
    pub progress: &'a datalib_etl::progress::Progress,
}

impl ReplySearch<'_> {
    /// The plan for `targets`, carried out. The first error ends the
    /// search; what it settled before then is kept.
    pub async fn run(
        &self,
        targets: &[(String, String)],
        since: &str,
        top: &str,
        totals: &mut ReplySearchTotals,
    ) -> Result<()> {
        let mut conversations = Vec::with_capacity(targets.len());
        for (cid, _) in targets {
            let history = coverage::held(self.db.pool(), &db::history_scope(cid)).await?;
            conversations.push(Conversation {
                id: cid.clone(),
                history_held: !history.is_empty(),
                replies_held: coverage::held(self.db.pool(), &db::replies_scope(cid)).await?,
            });
        }
        let plan = plan(since, top, &conversations);
        if !plan.seed.is_empty() {
            let scopes: Vec<String> = plan.seed.iter().map(|c| db::replies_scope(c)).collect();
            self.db
                .store_reply_search_page(&[], &scopes, Some(&Span::new(since, top)))
                .await?;
        }
        for (gap, channels) in &plan.searches {
            for chunk in channels.chunks(CONVERSATIONS_PER_QUERY) {
                self.search(gap, chunk, totals).await?;
            }
        }
        info!(
            event = "slack_reply_search_done",
            seeded = plan.seed.len(),
            searches = plan.searches.len(),
            pages = totals.pages,
            replies_found = totals.replies_found,
            roots_reread = totals.roots_reread,
            "searched for new replies on threads history will not read again"
        );
        Ok(())
    }

    /// Every page of the search of `gap` in `channels`, newest first.
    /// Each page settles from its oldest message up to where the page
    /// before left off, as a history page does. A search that reaches
    /// Slack's last page before the bottom of `gap` is asked again below
    /// what it settled.
    async fn search(
        &self,
        gap: &Span,
        channels: &[String],
        totals: &mut ReplySearchTotals,
    ) -> Result<()> {
        let wanted: HashSet<&str> = channels.iter().map(String::as_str).collect();
        let scopes: Vec<String> = channels.iter().map(|c| db::replies_scope(c)).collect();
        let mut stretch = gap.clone();
        loop {
            let q = query(channels, &stretch);
            let mut top = Some(stretch.hi.clone());
            let mut page = 1u64;
            let reached_bottom = loop {
                self.progress.set_message(&format!(
                    "{M_SEARCH} page {page} ({} found)",
                    totals.replies_found
                ));
                let mut params = BTreeMap::new();
                params.insert("query".to_string(), q.clone());
                params.insert("count".to_string(), PER_PAGE.to_string());
                params.insert("sort".to_string(), "timestamp".to_string());
                params.insert("sort_dir".to_string(), "desc".to_string());
                params.insert("page".to_string(), page.to_string());
                let resp = call(M_SEARCH, &params, self.latchkey).await?;
                totals.pages += 1;
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
                let last = matches.is_empty() || page >= pages;
                let in_stretch: Vec<String> = matches
                    .iter()
                    .filter(|m| {
                        m.get("channel")
                            .and_then(|c| c.get("id"))
                            .and_then(Value::as_str)
                            .is_some_and(|c| wanted.contains(c))
                    })
                    .filter_map(|m| m.get("ts").and_then(Value::as_str).map(ts_key))
                    .filter(|key| *key >= stretch.lo && *key <= stretch.hi)
                    .collect();
                let covered = settled(&stretch, top.as_deref(), &in_stretch, last);
                let hits: Vec<ReplyHit> = matches
                    .iter()
                    .filter_map(reply_hit)
                    .filter(|h| wanted.contains(h.channel_id.as_str()))
                    .collect();
                totals.replies_found += hits.len();
                let roots = self.stale_roots(&hits).await?;
                totals.roots_reread += roots.len();
                self.db
                    .store_reply_search_page(&roots, &scopes, covered.as_ref())
                    .await?;
                if last {
                    break true;
                }
                if page >= MAX_PAGES {
                    break false;
                }
                if let Some(oldest) = in_stretch.into_iter().min() {
                    top = Some(oldest);
                }
                page += 1;
            };
            if reached_bottom {
                return Ok(());
            }
            match top.filter(|t| *t < stretch.hi && *t > stretch.lo) {
                Some(below) => stretch = Span::new(stretch.lo.clone(), below),
                None => anyhow::bail!(
                    "{M_SEARCH}: {MAX_PAGES} pages settled nothing of {}..{}; the rest was not searched",
                    key_ts(&stretch.lo),
                    key_ts(&stretch.hi)
                ),
            }
        }
    }

    /// The roots of `hits` whose stored `latest_reply` is older than a
    /// reply found, read again. A root not stored is left to the walk
    /// that will store it, fresh.
    async fn stale_roots(&self, hits: &[ReplyHit]) -> Result<Vec<MessageInput>> {
        let mut newest: BTreeMap<String, &ReplyHit> = BTreeMap::new();
        for h in hits {
            let key = slack_thread_key(self.team_id, &h.channel_id, &h.thread_ts);
            let e = newest.entry(key).or_insert(h);
            if ts_key(&h.ts) > ts_key(&e.ts) {
                *e = h;
            }
        }
        let keys: Vec<String> = newest.keys().cloned().collect();
        let stored = self.db.root_latest_replies(&keys).await?;
        let mut roots = Vec::new();
        for (key, hit) in newest {
            let Some(listed) = stored.get(&key) else {
                continue;
            };
            if listed
                .as_deref()
                .is_some_and(|l| ts_key(l) >= ts_key(&hit.ts))
            {
                continue;
            }
            if let Some(root) = self.read_root(&hit.channel_id, &hit.thread_ts).await? {
                roots.push(root);
            }
        }
        Ok(roots)
    }

    /// The one message at `thread_ts`, as history lists it.
    async fn read_root(&self, channel_id: &str, thread_ts: &str) -> Result<Option<MessageInput>> {
        let mut params = BTreeMap::new();
        params.insert("channel".to_string(), channel_id.to_string());
        params.insert("oldest".to_string(), thread_ts.to_string());
        params.insert("latest".to_string(), thread_ts.to_string());
        params.insert("inclusive".to_string(), "true".to_string());
        params.insert("include_all_metadata".to_string(), "true".to_string());
        params.insert("limit".to_string(), "1".to_string());
        let resp = call(M_HISTORY, &params, self.latchkey).await?;
        Ok(resp
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|m| m.get("ts").and_then(Value::as_str) == Some(thread_ts))
            .find_map(|m| history_message_input(self.team_id, channel_id, m)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(ts: &str) -> String {
        ts_key(ts)
    }

    fn conv(id: &str, history_held: bool, replies_held: &[(&str, &str)]) -> Conversation {
        Conversation {
            id: id.into(),
            history_held,
            replies_held: replies_held
                .iter()
                .map(|(lo, hi)| Span::new(key(lo), key(hi)))
                .collect(),
        }
    }

    const SINCE: &str = "1704067200.000000";
    const LAST_RUN: &str = "1790000000.000000";
    const TOP: &str = "1790259200.000000";

    /// A conversation never walked is read whole by the walk ahead, so
    /// its replies need no search; one walked before owes the reply time
    /// since its last search, and conversations owing the same stretch
    /// share it.
    #[test]
    fn a_fresh_conversation_is_seeded_and_the_rest_owe_what_is_new() {
        let p = plan(
            &key(SINCE),
            &key(TOP),
            &[
                conv("C1", true, &[(SINCE, LAST_RUN)]),
                conv("C2", false, &[]),
                conv("D1", true, &[(SINCE, LAST_RUN)]),
            ],
        );
        assert_eq!(p.seed, ["C2"]);
        assert_eq!(
            p.searches,
            [(
                Span::new(key(LAST_RUN), key(TOP)),
                vec!["C1".into(), "D1".into()]
            )]
        );
    }

    /// A store that predates the search has walked history and searched
    /// nothing: it owes a search back to `since`, once.
    #[test]
    fn a_store_never_searched_owes_everything_since_since() {
        let p = plan(&key(SINCE), &key(TOP), &[conv("C1", true, &[])]);
        assert_eq!(
            p.searches,
            [(Span::new(key(SINCE), key(TOP)), vec!["C1".into()])]
        );
        assert!(p.seed.is_empty());
    }

    /// Two runs with one now: the second owes nothing.
    #[test]
    fn nothing_is_owed_up_to_the_top_already_searched() {
        let p = plan(&key(SINCE), &key(TOP), &[conv("C1", true, &[(SINCE, TOP)])]);
        assert_eq!(p, Plan::default());
    }

    /// `after:`/`before:` exclude their day and are read in the account's
    /// time zone, so the query reaches two days past each end.
    #[test]
    fn the_query_names_every_conversation_and_pads_the_days() {
        let span = Span::new(key("1790000000.000000"), key("1790259200.000000"));
        assert_eq!(
            query(&["C1".into(), "D2".into()], &span),
            "is:thread in:<#C1> in:<#D2> after:2026-09-19 before:2026-09-26"
        );
    }

    #[test]
    fn a_reply_is_a_match_whose_permalink_names_another_thread() {
        let m =
            |ts: &str, link: &str| json!({"ts": ts, "channel": {"id": "C1"}, "permalink": link});
        assert_eq!(
            reply_hit(&m(
                "1790000100.000200",
                "https://ncc-1701.slack.com/archives/C1/p1790000100000200?thread_ts=1700000000.000100&cid=C1"
            )),
            Some(ReplyHit {
                channel_id: "C1".into(),
                ts: "1790000100.000200".into(),
                thread_ts: "1700000000.000100".into(),
            })
        );
        // A root lists its own `ts`; a message in no thread lists none.
        assert_eq!(
            reply_hit(&m(
                "1700000000.000100",
                "https://ncc-1701.slack.com/archives/C1/p1700000000000100?thread_ts=1700000000.000100&cid=C1"
            )),
            None
        );
        assert_eq!(
            reply_hit(&m(
                "1700000000.000100",
                "https://ncc-1701.slack.com/archives/C1/p1700000000000100"
            )),
            None
        );
    }
}
