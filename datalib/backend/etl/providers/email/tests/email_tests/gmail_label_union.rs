//! Gmail's `only_extract_labels` means "carrying **any** of these".
//!
//! Driven through the HTTP playback layer, so it runs in CI with no
//! credential and no network. The fixtures model Gmail's real behaviour,
//! measured live on 2026-09-08: repeated `labelIds` in one request
//! **intersect**, so asking for two labels at once returns only the
//! messages carrying both. That is what made a three-label config
//! download nothing at all while reporting success.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_list_url, gmail_message, inbox_label, put_gmail, put_gmail_account, Mirror,
};

/// Under `datalib` only.
const ONLY_LIB: &str = "18c9f2a1b2c3d401";
/// Under both labels — the one message the old intersecting request
/// would have returned.
const BOTH: &str = "18c9f2a1b2c3d402";
/// Under `travel` only.
const ONLY_TRAVEL: &str = "18c9f2a1b2c3d403";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mirrors_the_union_of_the_configured_labels() {
    let m = Mirror::new();
    write_fixtures(&m.playback);

    let summary = m
        .run(|db| {
            let mut opts = FetchOptions::new(db);
            opts.only_labels = vec!["datalib".to_string(), "travel".to_string()];
            gmail_api::fetch(opts)
        })
        .await;

    let summary = summary.expect("gmail fetch under playback");
    assert!(
        summary.full_sync,
        "no stored cursor, so this is a full sync"
    );
    assert_eq!(
        summary.emails_upserted, 3,
        "expected the union of both labels, got {summary:?}",
    );
    // The message under both labels is listed by both walks. Fetching it
    // twice would cost 20 quota units and write the same row again.
    assert_eq!(
        summary.blobs_stored, 3,
        "a message under two configured labels was fetched twice: {summary:?}",
    );

    assert_eq!(
        m.gmail_ids().await,
        BTreeSet::from([
            ONLY_LIB.to_string(),
            BOTH.to_string(),
            ONLY_TRAVEL.to_string(),
        ]),
        "a message under exactly one of the two labels was dropped — \
         the enumeration intersected the labels instead of unioning them",
    );
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(
        playback,
        "1000",
        json!([
            inbox_label(),
            { "id": "UNREAD", "name": "UNREAD", "type": "system" },
            { "id": "Label_7", "name": "datalib", "type": "user" },
            { "id": "Label_9", "name": "travel", "type": "user" },
        ]),
    );

    // What Gmail answers when both labels ride on one request: the
    // intersection, not the union. Present so that code combining them
    // fails this test on the row count rather than on a missing fixture.
    put_gmail(
        playback,
        &gmail_list_url(&["Label_7", "Label_9"]),
        &json!({ "messages": [{ "id": BOTH }] }),
    );

    put_gmail(
        playback,
        &gmail_list_url(&["Label_7"]),
        &json!({ "messages": [{ "id": ONLY_LIB }, { "id": BOTH }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["Label_9"]),
        &json!({ "messages": [{ "id": BOTH }, { "id": ONLY_TRAVEL }] }),
    );

    for (id, labels, subject) in [
        (ONLY_LIB, &["Label_7"][..], "lib"),
        (BOTH, &["Label_7", "Label_9"][..], "both"),
        (ONLY_TRAVEL, &["Label_9"][..], "travel"),
    ] {
        put_gmail(
            playback,
            &gmail_get_url(id),
            &gmail_message(id, labels, subject),
        );
    }
}
