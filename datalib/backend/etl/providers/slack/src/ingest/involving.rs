//! `scope = "involves_me"`: the channels where something involves the
//! account, found by `search.messages`, and the stretches of each a run
//! walks. INGEST.md § "Only what involves me" has the rules.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::Result;
use chrono::Duration;
use serde_json::{json, Value};

use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl_slack_config::InvolvesMe;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use tracing::info;

use super::db::{self, RawDb};
use super::schema_raw::{involving_key, InvolvingRow};
use super::search::{days_clause, permalink_thread_ts, SearchWalk};
use super::{key_ts, ts_key, END_OF_TIME};

/// One search for messages that involve the account. What each is
/// written as was checked against the live API: `@me` matches nothing,
/// `<!here>` and `<!channel>` match the words, and a user group's
/// `<!subteam^…>` matches no mention of it, so user groups are left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Query {
    Mention,
    FromMe,
    Here,
    Channel,
}

impl Query {
    /// Its name in the `involving` table and in its `coverage` scope.
    pub fn name(self) -> &'static str {
        match self {
            Query::Mention => "mention",
            Query::FromMe => "from_me",
            Query::Here => "here",
            Query::Channel => "channel",
        }
    }

    fn text(self, user_id: &str) -> String {
        match self {
            Query::Mention => format!("<@{user_id}>"),
            Query::FromMe => format!("from:<@{user_id}>"),
            Query::Here => "@here".to_string(),
            Query::Channel => "@channel".to_string(),
        }
    }

    pub fn for_kinds(kinds: &[InvolvesMe]) -> Vec<Query> {
        let mut out: Vec<Query> = kinds
            .iter()
            .flat_map(|k| match k {
                InvolvesMe::Mention => &[Query::Mention][..],
                InvolvesMe::FromMe => &[Query::FromMe][..],
                InvolvesMe::Broadcast => &[Query::Here, Query::Channel][..],
            })
            .copied()
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// How much of a channel around each message that involves the account
/// a run walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// At most zero: how far before each message to start.
    pub before: Duration,
    /// How far after each message to walk; `None` is up to now.
    pub after: Option<Duration>,
}

/// A `ts` key moved by `by`, never before the epoch.
fn shift(key: &str, by: Duration) -> String {
    let ts = key_ts(key);
    let (secs, frac) = ts.split_once('.').unwrap_or((ts.as_str(), ""));
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
    let micros = secs
        .parse::<i64>()
        .unwrap_or(0)
        .saturating_mul(1_000_000)
        .saturating_add(frac.parse::<i64>().unwrap_or(0));
    let moved = micros
        .saturating_add(by.num_microseconds().unwrap_or(if by < Duration::zero() {
            i64::MIN
        } else {
            i64::MAX
        }))
        .max(0);
    ts_key(&format!("{}.{:06}", moved / 1_000_000, moved % 1_000_000))
}

/// A message a search listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Hit {
    pub channel_id: String,
    pub ts: String,
    /// The thread it is in, its root's `ts`: its own for a root.
    pub thread_ts: Option<String>,
}

/// What [`plan`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// The stretches of each channel to walk, as `ts` keys, lowest first.
    pub wanted: BTreeMap<String, Vec<Span>>,
    /// `(channel_id, thread_ts)` of each thread a reply that involves
    /// the account is in whose root no stretch reaches: a walk will not
    /// list the root, so it is read on its own.
    pub roots: Vec<(String, String)>,
}

/// The stretches of each of `channels` to walk: around each message
/// `hits` lists in it, and up to now from the `ts` `mentioned` gives
/// it, a mention `client.counts` shows that search may not yet. Nothing
/// starts before `since`, a `ts` key.
pub(super) fn plan(
    since: &str,
    hits: &[Hit],
    mentioned: &BTreeMap<String, String>,
    channels: &HashSet<String>,
    context: &Context,
) -> Plan {
    let start = |key: &str| shift(key, context.before).max(since.to_string());
    let mut spans: BTreeMap<String, Vec<Span>> = BTreeMap::new();
    let mut replies: BTreeSet<(String, String)> = BTreeSet::new();
    for hit in hits {
        let key = ts_key(&hit.ts);
        if key.as_str() < since || !channels.contains(&hit.channel_id) {
            continue;
        }
        let hi = match context.after {
            Some(after) => shift(&key, after),
            None => END_OF_TIME.to_string(),
        };
        spans
            .entry(hit.channel_id.clone())
            .or_default()
            .push(Span::new(start(&key), hi));
        if let Some(root) = hit.thread_ts.as_ref().filter(|t| **t != hit.ts) {
            replies.insert((hit.channel_id.clone(), root.clone()));
        }
    }
    for (channel_id, ts) in mentioned {
        if channels.contains(channel_id) {
            spans
                .entry(channel_id.clone())
                .or_default()
                .push(Span::new(start(&ts_key(ts)), END_OF_TIME));
        }
    }
    let wanted: BTreeMap<String, Vec<Span>> = spans
        .into_iter()
        .map(|(channel_id, spans)| (channel_id, coverage::merged(spans)))
        .collect();
    let roots = replies
        .into_iter()
        .filter(|(channel_id, root)| {
            let key = ts_key(root);
            !wanted[channel_id]
                .iter()
                .any(|s| s.lo <= key && key <= s.hi)
        })
        .collect();
    Plan { wanted, roots }
}

/// The `ts` each channel `client.counts` shows a mention of the account
/// in should be walked up to now from: the last message read, for a
/// mention there; now, for one in a thread, whose root the search will
/// find.
pub(super) fn mentioned(counts: &Value, now: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for e in counts
        .get("channels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mentions = e.get("mention_count").and_then(Value::as_u64).unwrap_or(0);
        let (Some(id), Some(last_read)) = (
            e.get("id").and_then(Value::as_str),
            e.get("last_read").and_then(Value::as_str),
        ) else {
            continue;
        };
        if mentions > 0 {
            out.insert(id.to_string(), last_read.to_string());
        }
    }
    for (id, n) in counts
        .pointer("/threads/mention_count_by_channel")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if n.as_u64().unwrap_or(0) > 0 {
            out.entry(id.clone()).or_insert_with(|| now.to_string());
        }
    }
    out
}

/// What [`InvolvingSearch::run`] did.
#[derive(Debug, Default)]
pub struct InvolvingTotals {
    pub pages: u64,
    pub found: usize,
}

pub(super) struct InvolvingSearch<'a> {
    pub db: &'a RawDb,
    pub team_id: &'a str,
    pub user_id: &'a str,
    pub latchkey: &'a LatchkeySettings,
    pub progress: &'a datalib_etl::progress::Progress,
}

impl InvolvingSearch<'_> {
    /// Each of `queries` over the part of `[since, top]` it has not
    /// searched, newest first. The first error ends the search; what it
    /// settled before then is kept.
    pub async fn run(
        &self,
        queries: &[Query],
        since: &str,
        top: &str,
        totals: &mut InvolvingTotals,
    ) -> Result<()> {
        if top <= since {
            return Ok(());
        }
        let wanted = Span::new(since, top);
        for &query in queries {
            let scope = db::involving_scope(query.name());
            let held = coverage::held(self.db.pool(), &scope).await?;
            for gap in coverage::gaps(&wanted, &held).into_iter().rev() {
                let text = query.text(self.user_id);
                let mut walk = SearchWalk::new(&gap, |stretch: &Span| {
                    format!("{text} {}", days_clause(stretch))
                });
                while let Some(page) = walk
                    .next(|_| true, self.latchkey, self.progress, query.name())
                    .await?
                {
                    totals.pages += 1;
                    let rows: Vec<InvolvingRow> = page
                        .matches
                        .iter()
                        .filter_map(|m| self.row(query, m))
                        .collect();
                    totals.found += rows.len();
                    self.db
                        .store_involving_page(&rows, &scope, page.covered.as_ref())
                        .await?;
                }
            }
        }
        info!(
            event = "slack_involving_search_done",
            queries = queries.len(),
            pages = totals.pages,
            found = totals.found,
            "searched for the messages that involve the account"
        );
        Ok(())
    }

    fn row(&self, query: Query, m: &Value) -> Option<InvolvingRow> {
        let ts = m.get("ts")?.as_str()?;
        let channel_id = m.get("channel")?.get("id")?.as_str()?;
        let thread_ts = permalink_thread_ts(m);
        let payload = json!({
            "ts": ts,
            "channel": {"id": channel_id},
            "permalink": m.get("permalink"),
        });
        Some(InvolvingRow {
            id_and_payload: WirePayload {
                id: involving_key(query.name(), self.team_id, channel_id, ts),
                payload: payload.to_string(),
            },
            query: query.name().to_string(),
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
            thread_ts: thread_ts.map(String::from),
        })
    }
}

/// The messages `queries` listed, as [`Hit`]s.
pub(super) async fn hits(db: &RawDb, queries: &[Query]) -> Result<Vec<Hit>> {
    let names: Vec<&str> = queries.iter().map(|q| q.name()).collect();
    Ok(db
        .involving(&names)
        .await?
        .into_iter()
        .map(|(channel_id, ts, thread_ts)| Hit {
            channel_id,
            ts,
            thread_ts,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINCE: &str = "1790000000.000000";
    const DAY: i64 = 86_400;

    fn key(ts: &str) -> String {
        ts_key(ts)
    }

    fn at(secs: i64) -> String {
        format!("{secs}.000100")
    }

    fn hit(channel: &str, ts: &str, thread_ts: Option<&str>) -> Hit {
        Hit {
            channel_id: channel.into(),
            ts: ts.into(),
            thread_ts: thread_ts.map(String::from),
        }
    }

    fn channels(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    const WIDE: Context = Context {
        before: Duration::days(-1),
        after: None,
    };

    #[test]
    fn shift_moves_a_key_and_stops_at_the_epoch() {
        assert_eq!(
            shift(&key("1790000000.000100"), Duration::hours(-1)),
            key("1789996400.000100")
        );
        assert_eq!(
            shift(&key("1790000000.999999"), Duration::microseconds(1)),
            key("1790000001.000000")
        );
        assert_eq!(
            shift(&key("100.000000"), Duration::days(-1)),
            key("0.000000")
        );
    }

    /// The wide default: a channel is walked from a day before its
    /// oldest hit up to now; a channel with no hit is not walked.
    #[test]
    fn a_channel_is_walked_from_a_day_before_its_oldest_hit() {
        let first = 1_790_100_000;
        let p = plan(
            &key(SINCE),
            &[
                hit("C1", &at(first + 5 * DAY), None),
                hit("C1", &at(first), None),
            ],
            &BTreeMap::new(),
            &channels(&["C1", "C2"]),
            &WIDE,
        );
        assert_eq!(
            p.wanted,
            BTreeMap::from([(
                "C1".to_string(),
                vec![Span::new(key(&at(first - DAY)), END_OF_TIME)]
            )])
        );
        assert!(p.roots.is_empty());
    }

    /// No stretch starts before `since`, and a hit older than it counts
    /// for nothing.
    #[test]
    fn since_bounds_every_stretch() {
        let since: i64 = 1_790_000_000;
        let p = plan(
            &key(SINCE),
            &[
                hit("C1", &at(since + 3600), None),
                hit("C2", &at(since - 2 * DAY), None),
            ],
            &BTreeMap::new(),
            &channels(&["C1", "C2"]),
            &WIDE,
        );
        assert_eq!(
            p.wanted,
            BTreeMap::from([("C1".to_string(), vec![Span::new(key(SINCE), END_OF_TIME)])])
        );
    }

    /// With `context_after`, each hit gets its own window, and windows
    /// that touch become one.
    #[test]
    fn context_after_walks_a_window_around_each_hit() {
        let h = 1_790_100_000;
        let ctx = Context {
            before: Duration::hours(-3),
            after: Some(Duration::hours(6)),
        };
        let p = plan(
            &key(SINCE),
            &[
                hit("C1", &at(h), None),
                hit("C1", &at(h + 3600), None),
                hit("C1", &at(h + 10 * DAY), None),
            ],
            &BTreeMap::new(),
            &channels(&["C1"]),
            &ctx,
        );
        assert_eq!(
            p.wanted["C1"],
            vec![
                Span::new(key(&at(h - 3 * 3600)), key(&at(h + 7 * 3600))),
                Span::new(
                    key(&at(h + 10 * DAY - 3 * 3600)),
                    key(&at(h + 10 * DAY + 6 * 3600))
                ),
            ]
        );
    }

    /// A reply whose root is older than any stretch has its root read on
    /// its own; one whose root a stretch reaches does not.
    #[test]
    fn a_reply_on_a_root_no_stretch_reaches_has_its_root_read() {
        let h = 1_790_100_000;
        let old_root = at(h - 30 * DAY);
        let near_root = at(h - 3600);
        let p = plan(
            &key(SINCE),
            &[
                hit("C1", &at(h), Some(&old_root)),
                hit("C1", &at(h + 60), Some(&near_root)),
                hit("C1", &near_root, Some(&near_root)),
            ],
            &BTreeMap::new(),
            &channels(&["C1"]),
            &WIDE,
        );
        assert_eq!(p.roots, vec![("C1".to_string(), old_root)]);
    }

    /// A hit outside the channels this run may mirror (a DM, or a
    /// channel `channels` leaves out) chooses nothing.
    #[test]
    fn a_hit_outside_the_candidates_chooses_nothing() {
        let p = plan(
            &key(SINCE),
            &[hit("D1", &at(1_790_100_000), None)],
            &BTreeMap::new(),
            &channels(&["C1"]),
            &WIDE,
        );
        assert_eq!(p, Plan::default());
    }

    /// A mention `client.counts` shows opens the channel from before the
    /// last message read, whatever `context_after` says.
    #[test]
    fn a_counted_mention_walks_up_to_now() {
        let read = 1_790_100_000;
        let ctx = Context {
            before: Duration::hours(-3),
            after: Some(Duration::hours(1)),
        };
        let p = plan(
            &key(SINCE),
            &[],
            &BTreeMap::from([("C1".to_string(), at(read))]),
            &channels(&["C1"]),
            &ctx,
        );
        assert_eq!(
            p.wanted["C1"],
            vec![Span::new(key(&at(read - 3 * 3600)), END_OF_TIME)]
        );
    }

    #[test]
    fn mentioned_reads_channels_and_threads() {
        let counts = json!({
            "channels": [
                {"id": "C1", "mention_count": 2, "last_read": "1790000000.000100"},
                {"id": "C2", "mention_count": 0, "last_read": "1790000000.000100"},
            ],
            "ims": [{"id": "D1", "mention_count": 1, "last_read": "1790000000.000100"}],
            "threads": {"mention_count_by_channel": {"C3": 1, "C4": 0}},
        });
        assert_eq!(
            mentioned(&counts, "1790200000.000000"),
            BTreeMap::from([
                ("C1".to_string(), "1790000000.000100".to_string()),
                ("C3".to_string(), "1790200000.000000".to_string()),
            ])
        );
    }

    #[test]
    fn broadcast_is_two_searches() {
        assert_eq!(
            Query::for_kinds(&[InvolvesMe::Broadcast, InvolvesMe::Mention]),
            [Query::Mention, Query::Here, Query::Channel]
        );
        assert_eq!(Query::Mention.text("U1"), "<@U1>");
        assert_eq!(Query::FromMe.text("U1"), "from:<@U1>");
    }
}
