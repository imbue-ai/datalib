//! #1508: with `scope = "involves_me"`, search chooses the channels a
//! sync walks and where each starts; the walk is the one every sync
//! makes. Every channel's history is served, so a channel walked when it
//! should not be lands its message and fails the assertion.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_slack::ingest::{db_path_for, FetchOptions, InvolvesMeOptions};
use datalib_etl_slack::recorded::{record_call, History, CHANNEL_TYPES};
use datalib_etl_slack_config::InvolvesMe;
use serde_json::{json, Value};

use crate::support::{channels_with_messages, fetch_into, msg, serve, stored_ts};

/// 2026-10-01 and 2026-10-15T12:00Z: the search reaches from `since` to
/// half an hour before now, padded two days each way.
const SINCE: &str = "2026-10-01";
const SINCE_TS: &str = "1790812800.000000";
const NOW: &str = "2026-10-15T12:00:00Z";
const DAYS: &str = "after:2026-09-29 before:2026-10-17";

/// `#bridge`: a mention of the account, and an earlier message the same
/// day. Walked from a day before the mention.
const MENTION: &str = "1791244800.000100";
const BRIDGE_EARLIER: &str = "1791200000.000100";
const BRIDGE_FROM: &str = "1791158400.000100";

/// `#ten-forward`: the account's reply on a thread from before `since`,
/// and a message near it.
const ROOT: &str = "1780000000.000100";
const MY_REPLY: &str = "1791500000.000100";
const NEAR_MY_REPLY: &str = "1791450000.000100";
const TEN_FORWARD_FROM: &str = "1791413600.000100";

/// `#ops`: a mention `client.counts` shows and search has not listed,
/// after the last message read there.
const OPS_LAST_READ: &str = "1792000000.000100";
const OPS_FROM: &str = "1791913600.000100";
const OPS_MESSAGE: &str = "1792050000.000100";

/// `#engineering`: nothing in it involves the account.
const ENGINEERING: &str = "1791300000.000100";

fn search_match(channel: &str, ts: &str, thread_ts: Option<&str>) -> Value {
    let p = ts.replace('.', "");
    let thread = thread_ts.map_or(String::new(), |t| format!("?thread_ts={t}&cid={channel}"));
    json!({"ts": ts, "channel": {"id": channel}, "text": "",
           "permalink": format!("https://ncc-1701.slack.com/archives/{channel}/p{p}{thread}")})
}

fn record_search(api: &Path, query: &str, matches: Value) {
    let n = matches.as_array().map_or(0, Vec::len);
    record_call(
        api,
        "search.messages",
        json!({"query": format!("{query} {DAYS}"), "count": "100", "sort": "timestamp",
               "sort_dir": "desc", "page": "1"}),
        json!({"ok": true, "messages": {
            "total": n,
            "paging": {"count": 100, "total": n, "page": 1, "pages": if n > 0 { 1 } else { 0 }},
            "matches": matches,
        }}),
    )
    .unwrap();
}

/// The listings every sync makes: four channels, `client.counts` showing
/// the mention in `#ops`.
fn record_listings(api: &Path) {
    datalib_etl_slack::recorded::record_auth(api).unwrap();
    datalib_etl_slack::recorded::record_users(api, json!([{"id": "U1", "name": "picard"}]))
        .unwrap();
    datalib_etl_slack::recorded::record_conversations(
        api,
        CHANNEL_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true},
            {"id": "C2", "name": "engineering", "is_member": true},
            {"id": "C3", "name": "ten-forward", "is_member": true},
            {"id": "C4", "name": "ops", "is_member": true},
        ]),
    )
    .unwrap();
    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "mpims": [], "ims": [], "channels": [
            {"id": "C4", "mention_count": 1, "last_read": OPS_LAST_READ, "has_unreads": true},
        ]}),
    )
    .unwrap();
    // Served on every sync: a walk of `#engineering` would store it.
    History::from("C2", SINCE_TS)
        .record(api, json!([msg(ENGINEERING, "warp core nominal")]))
        .unwrap();
}

fn record_searches(api: &Path) {
    record_search(api, "<@U1>", json!([search_match("C1", MENTION, None)]));
    record_search(
        api,
        "from:<@U1>",
        json!([
            search_match("C3", MY_REPLY, Some(ROOT)),
            // A DM: `dms` decides those, and it is off.
            search_match("D9", "1791600000.000100", None),
        ]),
    );
    record_search(api, "@here", json!([]));
    record_search(api, "@channel", json!([]));
}

fn root(latest_reply: &str) -> Value {
    json!({"ts": ROOT, "user": "U2", "text": "poker night", "thread_ts": ROOT,
           "reply_count": 1, "latest_reply": latest_reply})
}

/// What the first sync reads: each chosen channel from its start, the
/// root no walk reaches, and its thread.
fn record_first_walks(api: &Path) {
    History::from("C1", BRIDGE_FROM)
        .record(
            api,
            json!([
                msg(MENTION, "<@U1> to the bridge"),
                msg(BRIDGE_EARLIER, "red alert")
            ]),
        )
        .unwrap();
    History::from("C3", TEN_FORWARD_FROM)
        .record(api, json!([msg(NEAR_MY_REPLY, "who deals?")]))
        .unwrap();
    History::from("C4", OPS_FROM)
        .record(api, json!([msg(OPS_MESSAGE, "<@U1> the shuttle is ready")]))
        .unwrap();
    record_call(
        api,
        "conversations.history",
        json!({"channel": "C3", "oldest": ROOT, "latest": ROOT, "inclusive": "true",
               "include_all_metadata": "true", "limit": "1"}),
        json!({"ok": true, "messages": [root(MY_REPLY)], "has_more": false}),
    )
    .unwrap();
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C3", "ts": ROOT, "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": [
            root(MY_REPLY),
            {"ts": MY_REPLY, "user": "U1", "text": "I'll deal", "thread_ts": ROOT},
        ]}),
    )
    .unwrap();
}

/// What any later sync asks of a chosen channel: anything newer than
/// the newest message held.
fn record_top_ups(api: &Path) {
    for (c, newest) in [("C1", MENTION), ("C3", NEAR_MY_REPLY), ("C4", OPS_MESSAGE)] {
        History {
            inclusive: false,
            ..History::from(c, newest)
        }
        .record(api, json!([]))
        .unwrap();
    }
}

fn involves_me(o: FetchOptions) -> FetchOptions {
    FetchOptions {
        since: SINCE.into(),
        now: NOW.parse().unwrap(),
        involves_me: Some(InvolvesMeOptions {
            kinds: vec![
                InvolvesMe::Mention,
                InvolvesMe::FromMe,
                InvolvesMe::Broadcast,
            ],
            before: chrono::Duration::days(-1),
            after: None,
        }),
        ..o
    }
}

async fn sync(
    api: &Path,
    out: &Path,
    adjust: impl FnOnce(FetchOptions) -> FetchOptions,
) -> anyhow::Result<()> {
    let playback = tempfile::tempdir().unwrap();
    serve(api, playback.path());
    fetch_into(playback.path(), out, adjust).await.map(|_| ())
}

async fn history_scopes(out: &Path) -> BTreeSet<String> {
    let reader = datalib_pin::open_reader(&db_path_for(out)).await.unwrap();
    let scopes: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT scope FROM coverage WHERE scope LIKE 'history:%'")
            .fetch_all(&reader)
            .await
            .unwrap();
    reader.close().await;
    scopes.into_iter().collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// The headline. The first sync walks `#bridge` and `#ten-forward`,
/// where search found the account, and `#ops`, where `client.counts`
/// shows a mention, each from a day before; it reads the root of the
/// account's reply, which no walk reaches, and the thread under it.
/// `#engineering` costs nothing. The second sync searches nothing again.
/// The third, with the scope back to every conversation, walks
/// `#engineering` and the rest of each channel down to `since`, keeping
/// everything the first stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_chooses_the_channels_and_where_each_starts() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");

    let first = dir.path().join("first");
    record_listings(&first);
    record_searches(&first);
    record_first_walks(&first);
    sync(&first, &out, involves_me).await.unwrap();
    assert_eq!(channels_with_messages(&out), set(&["C1", "C3", "C4"]));
    assert_eq!(
        stored_ts(&out),
        [
            ROOT,
            BRIDGE_EARLIER,
            MENTION,
            NEAR_MY_REPLY,
            MY_REPLY,
            OPS_MESSAGE
        ]
    );
    assert_eq!(
        history_scopes(&out).await,
        set(&["history:C1", "history:C3", "history:C4"]),
        "a channel nothing chose must not be covered: nothing read it"
    );

    // No search is recorded: one would fail the sync.
    let second = dir.path().join("second");
    record_listings(&second);
    record_top_ups(&second);
    sync(&second, &out, involves_me).await.unwrap();
    assert_eq!(channels_with_messages(&out), set(&["C1", "C3", "C4"]));

    let third = dir.path().join("third");
    record_listings(&third);
    record_top_ups(&third);
    for (c, from) in [
        ("C1", BRIDGE_FROM),
        ("C3", TEN_FORWARD_FROM),
        ("C4", OPS_FROM),
    ] {
        History {
            latest: Some(from),
            ..History::from(c, SINCE_TS)
        }
        .record(&third, json!([]))
        .unwrap();
    }
    sync(&third, &out, |o| FetchOptions {
        since: SINCE.into(),
        now: NOW.parse().unwrap(),
        ..o
    })
    .await
    .unwrap();
    assert_eq!(channels_with_messages(&out), set(&["C1", "C2", "C3", "C4"]));
    assert_eq!(stored_ts(&out).len(), 7);
}

/// A token that may not search cannot choose any channel. Walking every
/// channel instead is the cost the scope exists to avoid, so the sync
/// fails, and walks nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_search_fails_the_sync() {
    let dir = tempfile::tempdir().unwrap();
    let (api, out) = (dir.path().join("api"), dir.path().join("out"));
    record_listings(&api);
    record_call(
        &api,
        "search.messages",
        json!({"query": format!("<@U1> {DAYS}"), "count": "100", "sort": "timestamp",
               "sort_dir": "desc", "page": "1"}),
        json!({"ok": false, "error": "not_allowed_token_type"}),
    )
    .unwrap();

    let err = sync(&api, &out, involves_me).await.unwrap_err();
    assert!(format!("{err:#}").contains("involves_me"), "{err:#}");
    assert!(channels_with_messages(&out).is_empty());
}
