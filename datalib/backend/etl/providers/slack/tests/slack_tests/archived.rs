//! An archived channel is mirrored when the source asks for archived
//! channels, or names it, and not otherwise.

use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::json;

use crate::support::{channels_with_messages, fetch_into, msg, Tree};

/// `#bridge` (`C1`) is open; `#stargazer` (`C2`) was archived with
/// messages in it. Slack leaves it out of a listing that excludes
/// archived channels.
fn workspace() -> Tree {
    let t = Tree::new();
    record_auth(&t.api).unwrap();
    record_users(&t.api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    let bridge = json!({"id": "C1", "name": "bridge", "is_member": true, "is_archived": false});
    let stargazer =
        json!({"id": "C2", "name": "stargazer", "is_member": true, "is_archived": true});
    record_conversations(&t.api, CHANNEL_TYPES, json!([bridge])).unwrap();
    record_call(
        &t.api,
        "conversations.list",
        json!({"exclude_archived": "false", "limit": "1000", "types": CHANNEL_TYPES}),
        json!({"ok": true, "channels": [bridge, stargazer], "has_more": false}),
    )
    .unwrap();
    History::cold("C1")
        .record(&t.api, json!([msg("1735689600.000100", "Engage.")]))
        .unwrap();
    History::cold("C2")
        .record(&t.api, json!([msg("1735689600.000200", "Captain's log.")]))
        .unwrap();
    t.serve();
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_channel_is_mirrored_only_when_asked_for() {
    let t = workspace();
    let scratch = tempfile::tempdir().unwrap();
    let sync = |name: &str, adjust: fn(FetchOptions) -> FetchOptions| {
        let out = scratch.path().join(name);
        let playback = t.playback.clone();
        async move {
            fetch_into(&playback, &out, adjust).await.unwrap();
            channels_with_messages(&out)
        }
    };

    let open_only = sync("default", |o| o).await;
    assert_eq!(open_only, ["C1".to_string()].into());

    let archived = sync("archived", |o| FetchOptions {
        archived: true,
        ..o
    })
    .await;
    assert_eq!(archived, ["C1".to_string(), "C2".to_string()].into());

    let named = sync("named", |o| FetchOptions {
        channels: Some(vec!["stargazer".into()]),
        ..o
    })
    .await;
    assert_eq!(named, ["C2".to_string()].into());
}
