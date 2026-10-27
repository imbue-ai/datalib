//! `gmail.since` on a Gmail mirror: a walk lists only the mail after
//! it, an earlier `since` walks again and backfills, and nothing before
//! the window is deleted for not being listed. A walk of the whole
//! account with no window deletes what it does not name, so a windowed
//! walk that did the same would empty the mirror of everything older.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, put_gmail_history_expired, Mirror,
};

const SEPTEMBER: &str = "2026-09-01T00:00:00+00:00";
const AUGUST: &str = "2026-08-01T00:00:00+00:00";

const IN_SEPTEMBER: &str = "18c9f2a1b2c3d701";
const IN_AUGUST: &str = "18c9f2a1b2c3d702";
const IN_JULY: &str = "18c9f2a1b2c3d703";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_lists_its_stretch_and_an_earlier_start_backfills() {
    let h = Harness::new();
    let first = h.run(Some(SEPTEMBER)).await;
    assert_eq!(first.walked, vec!["*".to_string()]);
    assert_eq!(h.mirrored().await, ids(&[IN_SEPTEMBER]));

    let again = h.run(Some(SEPTEMBER)).await;
    assert!(again.walked.is_empty(), "{again:?}");
    assert_eq!(again.quota_units_spent, 4, "{again:?}");

    let widened = h.run(Some(AUGUST)).await;
    assert_eq!(widened.walked, vec!["*".to_string()], "{widened:?}");
    assert_eq!(widened.emails_upserted, 1, "{widened:?}");
    assert_eq!(h.mirrored().await, ids(&[IN_SEPTEMBER, IN_AUGUST]));

    let narrowed = h.run(Some(SEPTEMBER)).await;
    assert!(narrowed.walked.is_empty(), "{narrowed:?}");
    assert_eq!(
        h.mirrored().await,
        ids(&[IN_SEPTEMBER, IN_AUGUST]),
        "a narrower window keeps what is mirrored"
    );

    let unwindowed = h.run(None).await;
    assert_eq!(unwindowed.walked, vec!["*".to_string()], "{unwindowed:?}");
    assert_eq!(h.mirrored().await, ids(&[IN_SEPTEMBER, IN_AUGUST, IN_JULY]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_windowed_walk_deletes_nothing_before_the_window() {
    let h = Harness::new();
    h.run(None).await;
    assert_eq!(h.mirrored().await, ids(&[IN_SEPTEMBER, IN_AUGUST, IN_JULY]));

    h.lose_the_cursor();
    let rewalked = h.run(Some(SEPTEMBER)).await;
    assert_eq!(rewalked.walked, vec!["*".to_string()], "{rewalked:?}");
    assert_eq!(rewalked.emails_destroyed, 0, "{rewalked:?}");
    assert_eq!(h.mirrored().await, ids(&[IN_SEPTEMBER, IN_AUGUST, IN_JULY]));
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn after(since: &str) -> String {
    let secs = datalib_time::parse_strict(since)
        .unwrap()
        .inner()
        .timestamp();
    format!("{}&q=after%3A{secs}", gmail_list_url(&[]))
}

struct Harness(Mirror);

impl Harness {
    fn new() -> Self {
        let m = Mirror::new();
        write_fixtures(&m.playback);
        Self(m)
    }

    async fn run(&self, since: Option<&str>) -> FetchSummary {
        let since: Option<IsoOffsetTimestamp> = since.map(|s| s.parse().unwrap());
        self.0
            .run(|db| {
                let mut opts = FetchOptions::new(db);
                opts.since = since;
                gmail_api::fetch(opts)
            })
            .await
            .expect("gmail fetch under playback")
    }

    fn lose_the_cursor(&self) {
        put_gmail_history_expired(&self.0.playback, "1000");
    }

    async fn mirrored(&self) -> BTreeSet<String> {
        self.0.gmail_ids().await
    }
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(playback, "1000", json!([inbox_label()]));
    put_gmail(
        playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    put_gmail(
        playback,
        &after(SEPTEMBER),
        &json!({ "messages": [{ "id": IN_SEPTEMBER }] }),
    );
    put_gmail(
        playback,
        &after(AUGUST),
        &json!({ "messages": [{ "id": IN_SEPTEMBER }, { "id": IN_AUGUST }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [
            { "id": IN_SEPTEMBER }, { "id": IN_AUGUST }, { "id": IN_JULY },
        ]}),
    );
    for id in [IN_SEPTEMBER, IN_AUGUST, IN_JULY] {
        put_gmail(
            playback,
            &gmail_get_url(id),
            &gmail_message(id, &["INBOX"], id),
        );
    }
}
