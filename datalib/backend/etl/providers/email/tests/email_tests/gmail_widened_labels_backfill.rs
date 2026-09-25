//! Widening `only_extract_labels` on an already-synced Gmail mirror.
//!
//! The historyId cursor answers "what changed since last run?", and mail
//! that already sat outside the old label filter never changed — so an
//! incremental run after a widening is a silent no-op unless the run
//! notices the filter moved and walks what is newly in scope. Found on
//! a real account on 2026-09-15: three labels, then no filter, and two
//! further syncs spent 4 quota units each and mirrored nothing.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, Mirror,
};

/// Under `datalib`, the label the mirror started with.
const UNDER_LIB: &str = "18c9f2a1b2c3d601";
/// Under `travel`, admitted by the second config.
const UNDER_TRAVEL: &str = "18c9f2a1b2c3d602";
/// Under neither user label; only an unfiltered walk lists it.
const INBOX_ONLY: &str = "18c9f2a1b2c3d603";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widened_filter_backfills_what_is_newly_in_scope() {
    // One test, three scenarios in sequence: `PLAYBACK_ENV` is
    // process-global, so as separate `#[tokio::test]`s they would race.
    an_unchanged_filter_replays_history_only().await;
    an_added_label_walks_just_that_label().await;
    a_removed_filter_walks_the_whole_account().await;
}

async fn an_unchanged_filter_replays_history_only() {
    let h = Harness::new();
    let first = h.run(&["datalib"]).await;
    assert!(first.full_sync);
    assert_eq!(first.emails_upserted, 1, "{first:?}");

    let second = h.run(&["datalib"]).await;
    assert!(!second.full_sync, "{second:?}");
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert!(second.backfilled_labels.is_empty(), "{second:?}");
    // profile + labels + history.list: the run must not have walked.
    assert_eq!(second.quota_units_spent, 4, "{second:?}");
    assert_eq!(h.mirrored().await, ids(&[UNDER_LIB]));
}

async fn an_added_label_walks_just_that_label() {
    let h = Harness::new();
    h.run(&["datalib"]).await;

    let widened = h.run(&["datalib", "travel"]).await;
    assert!(!widened.full_sync, "the cursor still stands: {widened:?}");
    assert_eq!(widened.backfilled_labels, vec!["travel".to_string()]);
    assert_eq!(widened.emails_upserted, 1, "{widened:?}");
    assert_eq!(h.mirrored().await, ids(&[UNDER_LIB, UNDER_TRAVEL]));

    // The filter is recorded once satisfied, so the next run is quiet.
    let again = h.run(&["datalib", "travel"]).await;
    assert!(again.backfilled_labels.is_empty(), "{again:?}");
    assert_eq!(again.emails_upserted, 0, "{again:?}");
}

async fn a_removed_filter_walks_the_whole_account() {
    let h = Harness::new();
    h.run(&["datalib"]).await;

    let widened = h.run(&[]).await;
    assert!(!widened.full_sync, "the cursor still stands: {widened:?}");
    assert_eq!(widened.backfilled_labels, vec!["*".to_string()]);
    assert_eq!(
        widened.emails_upserted, 2,
        "the whole account was not walked: {widened:?}",
    );
    assert_eq!(
        h.mirrored().await,
        ids(&[UNDER_LIB, UNDER_TRAVEL, INBOX_ONLY])
    );

    let again = h.run(&[]).await;
    assert!(again.backfilled_labels.is_empty(), "{again:?}");
    assert_eq!(again.quota_units_spent, 4, "{again:?}");
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

struct Harness(Mirror);

impl Harness {
    fn new() -> Self {
        let m = Mirror::new();
        write_fixtures(&m.playback);
        Self(m)
    }

    async fn run(&self, labels: &[&str]) -> FetchSummary {
        self.0
            .run(|db| {
                let mut opts = FetchOptions::new(db);
                opts.only_labels = labels.iter().map(|s| s.to_string()).collect();
                gmail_api::fetch(opts)
            })
            .await
            .expect("gmail fetch under playback")
    }

    async fn mirrored(&self) -> BTreeSet<String> {
        self.0.gmail_ids().await
    }
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(
        playback,
        "1000",
        json!([
            inbox_label(),
            { "id": "Label_7", "name": "datalib", "type": "user" },
            { "id": "Label_9", "name": "travel", "type": "user" },
        ]),
    );
    // Nothing changed since the first run's cursor: the case where an
    // incremental run has nothing to say about mail it never mirrored.
    put_gmail(
        playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["Label_7"]),
        &json!({ "messages": [{ "id": UNDER_LIB }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["Label_9"]),
        &json!({ "messages": [{ "id": UNDER_TRAVEL }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [
            { "id": UNDER_LIB }, { "id": UNDER_TRAVEL }, { "id": INBOX_ONLY },
        ]}),
    );
    for (id, labels) in [
        (UNDER_LIB, &["INBOX", "Label_7"][..]),
        (UNDER_TRAVEL, &["INBOX", "Label_9"][..]),
        (INBOX_ONLY, &["INBOX"][..]),
    ] {
        put_gmail(playback, &gmail_get_url(id), &gmail_message(id, labels, id));
    }
}
