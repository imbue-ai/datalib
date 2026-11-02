//! An archived channel is mirrored when the source asks for archived
//! channels, or names it, and not otherwise. Once mirrored it is not
//! asked for anything new: it takes no new messages. Its archived flag
//! follows Slack's listing, so one unarchived is walked again and one
//! archived later stops being walked.

use std::path::Path;

use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::{
    record_auth, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::json;

use crate::support::{channels_with_messages, fetch_into, msg, stored_ts, Tree};

/// `#bridge` (`C1`)'s one message, and `#stargazer` (`C2`)'s.
const BRIDGE: &str = "1735689600.000100";
const STARGAZER: &str = "1735689600.000200";
/// A message in `#stargazer` newer than the first sync, which a run
/// stores only if it asks `#stargazer` for what is newer.
const LATER: &str = "1735689700.000300";

/// The listing, archived channels marked as Slack marks them.
fn record_listing(api: &Path, stargazer_archived: bool) {
    let bridge = json!({"id": "C1", "name": "bridge", "is_member": true, "is_archived": false});
    let stargazer = json!({"id": "C2", "name": "stargazer", "is_member": true,
                           "is_archived": stargazer_archived});
    record_conversations(api, CHANNEL_TYPES, json!([bridge, stargazer])).unwrap();
}

/// The first sync's tape: each channel read from `since`.
fn first(stargazer_archived: bool) -> Tree {
    let t = Tree::new();
    record_auth(&t.api).unwrap();
    record_users(&t.api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_listing(&t.api, stargazer_archived);
    History::cold("C1")
        .record(&t.api, json!([msg(BRIDGE, "Engage.")]))
        .unwrap();
    History::cold("C2")
        .record(&t.api, json!([msg(STARGAZER, "Captain's log.")]))
        .unwrap();
    t.serve();
    t
}

/// A later sync's tape: each channel asked for what is newer than the
/// first sync held, `#stargazer` answering with [`LATER`].
fn later(stargazer_archived: bool) -> Tree {
    let t = Tree::new();
    record_auth(&t.api).unwrap();
    record_users(&t.api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_listing(&t.api, stargazer_archived);
    let newer = |channel, oldest| History {
        inclusive: false,
        ..History::from(channel, oldest)
    };
    newer("C1", BRIDGE).record(&t.api, json!([])).unwrap();
    newer("C2", STARGAZER)
        .record(&t.api, json!([msg(LATER, "Recommissioned.")]))
        .unwrap();
    t.serve();
    t
}

fn archived(o: FetchOptions) -> FetchOptions {
    FetchOptions {
        archived: true,
        ..o
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_channel_is_mirrored_only_when_asked_for() {
    let t = first(true);
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

    let with_archived = sync("archived", archived).await;
    assert_eq!(with_archived, ["C1".to_string(), "C2".to_string()].into());

    let named = sync("named", |o| FetchOptions {
        channels: Some(vec!["stargazer".into()]),
        ..o
    })
    .await;
    assert_eq!(named, ["C2".to_string()].into());
}

/// On a real workspace with `all_channels`, `client.counts` listed one
/// of 469 archived channels, so each of the rest was asked for anything
/// newer on every sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mirrored_archived_channel_is_not_asked_again_until_unarchived() {
    let out = tempfile::tempdir().unwrap();
    let out = out.path();
    let held = |ts: &[&str]| ts.iter().map(|t| t.to_string()).collect::<Vec<_>>();

    fetch_into(&first(true).playback, out, archived)
        .await
        .unwrap();
    assert_eq!(stored_ts(out), held(&[BRIDGE, STARGAZER]));

    fetch_into(&later(true).playback, out, archived)
        .await
        .unwrap();
    assert_eq!(
        stored_ts(out),
        held(&[BRIDGE, STARGAZER]),
        "a second sync asked the archived channel for something newer"
    );

    fetch_into(&later(false).playback, out, archived)
        .await
        .unwrap();
    assert_eq!(
        stored_ts(out),
        held(&[BRIDGE, STARGAZER, LATER]),
        "an unarchived channel is walked again"
    );
}

/// The listing a source without `archived` sent left an archived
/// channel out, so its stored flag never changed and it was walked on
/// every sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_archived_after_it_was_mirrored_stops_being_walked() {
    let out = tempfile::tempdir().unwrap();
    let out = out.path();
    fetch_into(&first(false).playback, out, |o| o)
        .await
        .unwrap();
    fetch_into(&later(true).playback, out, |o| o).await.unwrap();
    assert!(
        !stored_ts(out).contains(&LATER.to_string()),
        "a channel archived since the last sync was walked"
    );
}
