//! Whole-download tests for what the account itself holds: read states,
//! saved-for-later items and channel bookmarks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use datalib_etl::doltlite_raw as dr;
use datalib_etl_slack::ingest::{db_path_for, FetchSummary, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{fetch_into, serve};

fn call(api: &Path, method: &str, params: Value, response: Value) {
    record_call(api, method, params, response).unwrap();
}

/// Two channels, only `C1` with a bookmarks bar, and a DM the config
/// (`dms = false`) never asks about.
fn write_workspace(api: &Path) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true, "is_archived": false,
             "properties": {"tabs": [{"id": "files", "type": "files", "label": ""},
                                     {"id": "bookmarks", "type": "bookmarks", "label": ""}]}},
            {"id": "C2", "name": "engineering", "is_member": true, "is_archived": false,
             "properties": {"tabs": [{"id": "files", "type": "files", "label": ""}]}},
        ]),
    )
    .unwrap();
    for channel in ["C1", "C2"] {
        History::cold(channel).record(api, json!([])).unwrap();
    }
    // Served so that asking about `C2` would store something: its
    // absence below then proves the bar was consulted, rather than a
    // missing fixture looking like a correct skip.
    for (channel, ids) in [("C1", &["B1", "B2"][..]), ("C2", &["B9"][..])] {
        let bookmarks: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({"id": id, "channel_id": channel, "type": "link",
                             "title": format!("bookmark {id}"), "link": "https://example.com"})
            })
            .collect();
        call(
            api,
            "bookmarks.list",
            json!({"channel_id": channel}),
            json!({"ok": true, "bookmarks": bookmarks}),
        );
    }
}

fn read_state(id: &str, last_read: &str) -> Value {
    json!({"id": id, "last_read": last_read, "latest": "1735689600.000900",
           "updated": "1735689601.000000", "history_invalid": "1735689600.000000",
           "mention_count": 0, "has_unreads": true})
}

fn write_counts(api: &Path, c1_last_read: &str) {
    call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true,
               "channels": [read_state("C1", c1_last_read), read_state("C2", "1735689600.000500")],
               "mpims": [],
               "ims": [read_state("D1", "1735689600.000600")]}),
    );
}

fn saved(item_id: &str, ts: &str, state: &str) -> Value {
    json!({"item_id": item_id, "item_type": "message", "ts": ts, "state": state,
           "todo_state": state, "is_archived": false, "date_created": 1735689600})
}

fn write_saved(api: &Path, saved_page: Vec<Value>) {
    let page = |filter: &str, cursor: Option<&str>, items: Vec<Value>, next: &str| {
        let mut params = json!({"filter": filter, "limit": "50"});
        if let Some(c) = cursor {
            params["cursor"] = json!(c);
        }
        call(
            api,
            "saved.list",
            params,
            json!({"ok": true, "saved_items": items,
                   "response_metadata": {"next_cursor": next}}),
        );
    };
    page("saved", None, saved_page, "");
    // Two pages, so a walk that stops at the first loses `C2 …0002`.
    page(
        "completed",
        None,
        vec![saved("C2", "1735689600.000001", "completed")],
        "page2",
    );
    page(
        "completed",
        Some("page2"),
        vec![saved("C2", "1735689600.000002", "completed")],
        "",
    );
    page("archived", None, vec![], "");
}

async fn run_fetch(out: &Path) -> FetchSummary {
    fetch_into(out, |o| o).await.unwrap()
}

async fn stored(out: &Path) -> (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let content = dr::load_payloads(
        db.pool(),
        datalib_etl::pin::Reads::Own,
        "channel_read_states",
    )
    .await
    .unwrap();
    let read_states = db.load_read_states().await.unwrap();
    let saved = db.load_saved_items().await.unwrap();
    let bookmarks = db.load_bookmarks().await.unwrap();
    db.close().await;
    (content, read_states, saved, bookmarks)
}

fn ids(values: &[Value], key: &str) -> BTreeSet<String> {
    values
        .iter()
        .map(|v| v[key].as_str().unwrap().to_string())
        .collect()
}

/// Read states, saved items and bookmarks land for the mirrored
/// conversations only; `last_read` lives in the volatile sidecar, so a
/// second run that finds it moved leaves the content table as it was;
/// and a saved item upstream no longer lists is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_states_saved_items_and_bookmarks_are_stored() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");

    let api1 = d.path().join("api1");
    write_workspace(&api1);
    write_counts(&api1, "1735689600.000100");
    write_saved(
        &api1,
        vec![
            saved("C1", "1735689600.000100", "in_progress"),
            // A DM this run does not mirror.
            saved("D1", "1735689600.000200", "in_progress"),
        ],
    );
    let playback1 = d.path().join("playback1");
    serve(&api1, &playback1);
    let summary = run_fetch(&out).await;
    assert_eq!(summary.account.read_states, 2);
    assert_eq!(summary.account.saved_items, 3);
    assert_eq!(summary.account.bookmarks, 2);

    let (content, read_states, saved_items, bookmarks) = stored(&out).await;
    assert_eq!(
        content,
        vec![json!({"id": "C1"}), json!({"id": "C2"})],
        "a read state's content is its id alone; the rest is volatile",
    );
    assert_eq!(
        ids(&read_states, "id"),
        BTreeSet::from(["C1".into(), "C2".into()]),
        "the DM's read state came through with dms off",
    );
    let c1 = read_states.iter().find(|r| r["id"] == "C1").unwrap();
    assert_eq!(c1["last_read"], "1735689600.000100");
    assert_eq!(c1["has_unreads"], true);
    assert_eq!(
        ids(&saved_items, "ts"),
        BTreeSet::from([
            "1735689600.000100".into(),
            "1735689600.000001".into(),
            "1735689600.000002".into(),
        ]),
        "the saved items should be every page of every filter, less the DM",
    );
    assert_eq!(
        ids(&bookmarks, "id"),
        BTreeSet::from(["B1".into(), "B2".into()]),
        "only the channel with a bookmarks bar should have been asked",
    );

    // Run 2: `C1` read further, and the saved `C1` message was unsaved.
    let api2 = d.path().join("api2");
    write_workspace(&api2);
    write_counts(&api2, "1735689600.000900");
    write_saved(&api2, vec![]);
    let playback2 = d.path().join("playback2");
    serve(&api2, &playback2);
    run_fetch(&out).await;

    let (content2, read_states2, saved_items2, _) = stored(&out).await;
    assert_eq!(content2, content, "reading moved the content payload");
    let c1 = read_states2.iter().find(|r| r["id"] == "C1").unwrap();
    assert_eq!(c1["last_read"], "1735689600.000900");
    assert_eq!(
        ids(&saved_items2, "ts"),
        BTreeSet::from(["1735689600.000001".into(), "1735689600.000002".into()]),
        "an unsaved message should leave the saved items",
    );
}

const A: &str = "1735689600.000100";
const REPLY: &str = "1735689600.000150";
const B: &str = "1735689600.000200";
const C: &str = "1735689600.000300";

/// `C1` with three top-level messages from Riker, the first of them a
/// thread the account follows (Slack's `replies` copy of its root carries
/// `last_read`), read up to `channel_mark`.
fn write_channel_with_marks(api: &Path, channel_mark: &str) {
    record_auth(api).unwrap();
    record_users(
        api,
        json!([{"id": "U1", "name": "picard"}, {"id": "U2", "name": "riker"}]),
    )
    .unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([
            {"id": "C1", "name": "bridge", "is_member": true, "is_archived": false},
        ]),
    )
    .unwrap();
    let root = json!({"ts": A, "user": "U2", "text": "status report", "thread_ts": A,
                      "reply_count": 1, "latest_reply": REPLY});
    History::cold("C1")
        .record(
            api,
            json!([
                {"ts": C, "user": "U2", "text": "third"},
                {"ts": B, "user": "U2", "text": "second"},
                root,
            ]),
        )
        .unwrap();
    History {
        inclusive: false,
        ..History::from("C1", C)
    }
    .record(api, json!([]))
    .unwrap();
    let mut followed_root = root.clone();
    followed_root["subscribed"] = json!(true);
    followed_root["last_read"] = json!(A);
    call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": A, "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": [
            followed_root,
            {"ts": REPLY, "user": "U2", "text": "all nominal", "thread_ts": A},
        ]}),
    );
    call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "channels": [read_state("C1", channel_mark)], "mpims": [], "ims": []}),
    );
    write_saved(api, vec![]);
}

async fn sync_to_commit(out: &Path, api: &Path, playback: &Path) -> String {
    serve(api, playback);
    run_fetch(out).await;
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let head = dr::head_commit(db.pool()).await.unwrap().expect("a commit");
    db.close().await;
    head
}

/// The markdown of each rendered thread, keyed by its root's text.
fn render_threads(
    raw: &Path,
    range: datalib_etl_render::inputs::RawRange<'_>,
) -> (
    datalib_etl_slack_render::render::ParsedSlack,
    BTreeMap<String, String>,
) {
    let parsed = datalib_etl_slack_render::render::parse(raw, "src", range).unwrap();
    let out = tempdir().unwrap();
    let mut docs = BTreeMap::new();
    datalib_etl_slack_render::render::render::render_all(
        &parsed,
        out.path(),
        "src",
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            let md: String = doc.sections.iter().map(|s| s.md.as_str()).collect();
            let root = ["status report", "second", "third"]
                .into_iter()
                .find(|t| md.contains(&format!("title: \"#bridge: {t}")))
                .unwrap_or("?");
            docs.insert(root.to_string(), md);
            Ok(())
        },
    )
    .unwrap();
    (parsed, docs)
}

fn unread_count(md: &str) -> usize {
    md.matches("msg--slack unread").count()
}

/// A top-level message past its conversation's mark, and a reply past
/// its followed thread's, render unread. When a later sync moves the
/// conversation's mark, the incremental render names the threads the
/// mark crossed — and not one that is unread on both sides of it, which
/// is the whole point of keeping the mark out of the content diff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_marks_render_and_a_moved_mark_rerenders_only_what_it_crossed() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");

    let api1 = d.path().join("api1");
    write_channel_with_marks(&api1, A);
    let first = sync_to_commit(&out, &api1, &d.path().join("playback1")).await;

    let (_, cold) = render_threads(&out, datalib_etl_render::inputs::RawRange::cold());
    assert_eq!(
        unread_count(&cold["status report"]),
        1,
        "the root is read, its reply past the thread's mark is not: {}",
        cold["status report"]
    );
    assert!(cold["status report"].contains("first-unread"));
    assert_eq!(unread_count(&cold["second"]), 1);
    assert_eq!(unread_count(&cold["third"]), 1);

    let api2 = d.path().join("api2");
    write_channel_with_marks(&api2, B);
    let second = sync_to_commit(&out, &api2, &d.path().join("playback2")).await;
    assert_ne!(first, second, "the moved mark is a commit of its own");

    let nothing_stale = std::collections::HashSet::new();
    let (parsed, incremental) = render_threads(
        &out,
        datalib_etl_render::inputs::RawRange {
            cursor: Some(&first),
            pin: Some(&second),
            stale: Some(&nothing_stale),
        },
    );
    let named = parsed.scan.render.expect("a narrowed render");
    assert!(named.contains(&format!("T1#C1#{B}")), "{named:?}");
    assert!(
        !named.contains(&format!("T1#C1#{C}")),
        "`third` is unread before and after; nothing about it moved: {named:?}"
    );
    assert_eq!(unread_count(&incremental["second"]), 0, "read now");
}
