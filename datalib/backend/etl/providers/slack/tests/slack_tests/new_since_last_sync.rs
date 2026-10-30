//! What appeared in the workspace after the last sync is found by the
//! next one, however soon it runs: a channel created since, a person
//! posting for the first time.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_slack::ingest::{block_on_load_all, db_path_for, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_conversations, record_user_info, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{channels_with_messages, fetch_into, msg, record_general, serve};

const A: &str = "1735689600.000100";
const B: &str = "1735689600.000200";

fn after<'a>(channel: &'a str, latest: &'a str) -> History<'a> {
    History {
        inclusive: false,
        ..History::from(channel, latest)
    }
}

fn channel(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "is_member": true, "is_archived": false})
}

/// `#general` holding Alice's one message, synced once into `out`.
async fn first_sync(root: &Path, out: &Path) {
    let api = root.join("api1");
    record_general(&api);
    History::cold("C1")
        .record(&api, json!([msg(A, "hello")]))
        .unwrap();
    let playback = root.join("playback1");
    serve(&api, &playback);
    fetch_into(&playback, out, |o| o).await.unwrap();
}

/// The second sync, minutes after the first: `record` adds its calls.
async fn second_sync(root: &Path, out: &Path, record: impl FnOnce(&Path)) {
    let api = root.join("api2");
    record_auth(&api).unwrap();
    record(&api);
    let playback = root.join("playback2");
    serve(&api, &playback);
    fetch_into(&playback, out, |o| o).await.unwrap();
}

async fn user_problems(out: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let keys = sqlx::query_scalar(
        "SELECT scope_key FROM problems WHERE scope_key LIKE 'users%' ORDER BY scope_key",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    keys
}

/// #1477: a channel created minutes after the first sync went unlisted
/// for six hours, so none of its messages reached the mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_created_since_the_last_sync_is_walked_by_the_next() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    first_sync(d.path(), &out).await;

    second_sync(d.path(), &out, |api| {
        record_conversations(
            api,
            CHANNEL_TYPES,
            json!([channel("C1", "general"), channel("C2", "launch")]),
        )
        .unwrap();
        after("C1", A).record(api, json!([])).unwrap();
        History::cold("C2")
            .record(api, json!([msg(B, "first post in launch")]))
            .unwrap();
    })
    .await;

    assert_eq!(
        channels_with_messages(&out),
        BTreeSet::from(["C1".to_string(), "C2".to_string()])
    );
}

/// #1477: a person whose first message came after the user listing had
/// no `users` row until that listing's six hours were up, so their
/// messages carried a bare id. The second sync records no `users.list`,
/// so it must not need one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_poster_is_looked_up_without_waiting_for_the_user_listing() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    first_sync(d.path(), &out).await;

    second_sync(d.path(), &out, |api| {
        record_conversations(api, CHANNEL_TYPES, json!([channel("C1", "general")])).unwrap();
        after("C1", A)
            .record(api, json!([{"ts": B, "user": "U2", "text": "first post"}]))
            .unwrap();
        record_user_info(
            api,
            json!({"id": "U2", "name": "riker",
                   "profile": {"real_name": "William Riker",
                               "fields": {"Xf01": {"value": "Bridge", "alt": ""}}}}),
        )
        .unwrap();
    })
    .await;

    let users = block_on_load_all(&db_path_for(&out)).unwrap().users;
    let riker = users
        .iter()
        .find(|u| u["id"] == "U2")
        .expect("the new poster has a users row");
    assert_eq!(riker["name"], "riker");
    assert_eq!(
        riker["profile"].get("fields"),
        None,
        "stored as users.list lists it, so the next listing is no change"
    );
    assert_eq!(
        user_problems(&out).await,
        Vec::<String>::new(),
        "only the unknown author is looked up, and it is found"
    );
}
