//! Part of a sync that fails is a `problems` row, not a failed step: the
//! rest of the run goes on, and the row goes once the same thing fetches.

use std::path::Path;

use datalib_etl_slack::ingest::{db_path_for, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{fetch_into, msg, serve, stored_ts};

const A: &str = "1735689600.000100";
const REPLY: &str = "1735689600.000150";
const B: &str = "1735689600.000200";

fn refused_by_slack() -> Value {
    json!({"ok": false, "error": "internal_error"})
}

/// The account-state listings, answered empty, so the only rows a test
/// sees are the ones it caused.
fn record_account(api: &Path) {
    record_call(
        api,
        "client.counts",
        json!({}),
        json!({"ok": true, "channels": [], "mpims": [], "ims": []}),
    )
    .unwrap();
    for filter in ["saved", "completed", "archived"] {
        record_call(
            api,
            "saved.list",
            json!({"filter": filter, "limit": "50"}),
            json!({"ok": true, "saved_items": []}),
        )
        .unwrap();
    }
}

fn record_listings(api: &Path, channels: Value) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(api, CHANNEL_TYPES, channels).unwrap();
    record_account(api);
}

fn channel(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "is_member": true, "is_archived": false})
}

/// The history call a second run sends for a channel whose newest stored
/// message is `latest`.
fn resumed<'a>(channel: &'a str, latest: &'a str) -> History<'a> {
    History {
        inclusive: false,
        ..History::from(channel, latest)
    }
}

async fn problems(out: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    rows
}

fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}

/// A channel whose history Slack will not give is an error row naming
/// it, and the other channel is mirrored all the same. The next run that
/// walks it clears the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_that_fails_is_a_problem_until_it_walks() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let channels = json!([channel("C1", "bridge"), channel("C2", "engineering")]);

    let api1 = d.path().join("api1");
    record_listings(&api1, channels.clone());
    History::cold("C1")
        .record(&api1, json!([msg(A, "status report")]))
        .unwrap();
    record_call(
        &api1,
        "conversations.history",
        json!({"channel": "C2", "include_all_metadata": "true", "inclusive": "true",
               "limit": "200", "oldest": History::cold("C2").oldest}),
        refused_by_slack(),
    )
    .unwrap();
    serve(&api1, &d.path().join("playback1"));
    fetch_into(&out, |o| o)
        .await
        .expect("one channel failing is not the run failing");
    assert_eq!(stored_ts(&out), [A]);
    assert_eq!(
        problems(&out).await,
        [row("listing:conversations.history engineering", "error")]
    );

    let api2 = d.path().join("api2");
    record_listings(&api2, channels);
    resumed("C1", A).record(&api2, json!([])).unwrap();
    History::cold("C2")
        .record(&api2, json!([msg(B, "warp core")]))
        .unwrap();
    serve(&api2, &d.path().join("playback2"));
    fetch_into(&out, |o| o).await.unwrap();
    assert_eq!(stored_ts(&out), [A, B]);
    assert_eq!(problems(&out).await, [], "the channel walked this time");
}

/// A thread whose replies will not come is a row on the thread's own
/// stamp, keyed like its root message so the render can name it, and the
/// channel is mirrored all the same. A later run asks for the thread
/// again though no walk lists its root, and the row goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_that_fails_is_a_problem_on_the_thread_until_it_fetches() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let channels = json!([channel("C1", "bridge")]);
    let replies_params = json!({"channel": "C1", "ts": A, "limit": "200"});

    let api1 = d.path().join("api1");
    record_listings(&api1, channels.clone());
    History::cold("C1")
        .record(
            &api1,
            json!([
                msg(B, "second"),
                {"ts": A, "user": "U1", "text": "status report", "thread_ts": A,
                 "reply_count": 1, "latest_reply": REPLY},
            ]),
        )
        .unwrap();
    record_call(
        &api1,
        "conversations.replies",
        replies_params.clone(),
        refused_by_slack(),
    )
    .unwrap();
    serve(&api1, &d.path().join("playback1"));
    fetch_into(&out, |o| o)
        .await
        .expect("one thread failing is not the run failing");
    assert_eq!(stored_ts(&out), [A, B]);
    assert_eq!(
        problems(&out).await,
        [row(&format!("replies_pages:T1#C1#{A}"), "error")],
        "its replies have never been read, so they are missing, not stale"
    );

    // The resumed walk lists nothing at or before `B`: the thread comes
    // back because the store holds a root whose replies it does not.
    let api2 = d.path().join("api2");
    record_listings(&api2, channels);
    resumed("C1", B).record(&api2, json!([])).unwrap();
    record_call(
        &api2,
        "conversations.replies",
        replies_params,
        json!({"ok": true, "has_more": false, "messages": [
            {"ts": A, "user": "U1", "text": "status report", "thread_ts": A,
             "reply_count": 1, "latest_reply": REPLY},
            {"ts": REPLY, "user": "U1", "text": "all nominal", "thread_ts": A},
        ]}),
    )
    .unwrap();
    serve(&api2, &d.path().join("playback2"));
    fetch_into(&out, |o| o).await.unwrap();
    assert_eq!(stored_ts(&out), [A, REPLY, B]);
    assert_eq!(problems(&out).await, [], "the thread fetched this time");
}

/// A user listing that fails costs the user directory, not the run; the
/// next run that lists it clears the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_listing_that_fails_is_a_problem_and_the_run_goes_on() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let channels = json!([channel("C1", "bridge")]);

    let api1 = d.path().join("api1");
    record_auth(&api1).unwrap();
    record_call(
        &api1,
        "users.list",
        json!({"limit": "200"}),
        refused_by_slack(),
    )
    .unwrap();
    record_conversations(&api1, CHANNEL_TYPES, channels.clone()).unwrap();
    record_account(&api1);
    History::cold("C1")
        .record(&api1, json!([msg(A, "status report")]))
        .unwrap();
    serve(&api1, &d.path().join("playback1"));
    fetch_into(&out, |o| o).await.unwrap();
    assert_eq!(stored_ts(&out), [A]);
    assert_eq!(problems(&out).await, [row("listing:users.list", "error")]);

    let api2 = d.path().join("api2");
    record_listings(&api2, channels);
    resumed("C1", A).record(&api2, json!([])).unwrap();
    serve(&api2, &d.path().join("playback2"));
    fetch_into(&out, |o| o).await.unwrap();
    assert_eq!(problems(&out).await, []);
}

/// A channel listing cut short walks the channels it did store, and
/// says the listing failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_listing_cut_short_walks_what_it_stored() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let api = d.path().join("api");
    record_auth(&api).unwrap();
    record_users(&api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    let page = json!({"exclude_archived": "true", "limit": "200", "types": CHANNEL_TYPES});
    record_call(
        &api,
        "conversations.list",
        page.clone(),
        json!({"ok": true, "channels": [channel("C1", "bridge")], "has_more": true,
               "response_metadata": {"next_cursor": "p2"}}),
    )
    .unwrap();
    let mut second_page = page;
    second_page["cursor"] = json!("p2");
    record_call(&api, "conversations.list", second_page, refused_by_slack()).unwrap();
    record_account(&api);
    History::cold("C1")
        .record(&api, json!([msg(A, "status report")]))
        .unwrap();
    serve(&api, &d.path().join("playback"));
    fetch_into(&out, |o| o).await.unwrap();
    assert_eq!(stored_ts(&out), [A]);
    assert_eq!(
        problems(&out).await,
        [row("listing:conversations.list", "error")]
    );
}

/// With no channel listed now or ever, there is nothing to walk: the one
/// listing failure that still fails the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_listing_that_fails_with_nothing_stored_fails_the_run() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let api = d.path().join("api");
    record_auth(&api).unwrap();
    record_users(&api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_call(
        &api,
        "conversations.list",
        json!({"exclude_archived": "true", "limit": "200", "types": CHANNEL_TYPES}),
        refused_by_slack(),
    )
    .unwrap();
    serve(&api, &d.path().join("playback"));
    let Err(err) = fetch_into(&out, |o| o).await else {
        panic!("the run walked nothing and still succeeded");
    };
    assert!(
        format!("{err:#}").contains("no channels are stored"),
        "{err:#}"
    );
}
