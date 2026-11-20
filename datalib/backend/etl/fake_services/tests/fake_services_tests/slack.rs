//! The Slack download against the fake Slack: what a first sync mirrors,
//! what a later sync picks up and what it costs, and how the download
//! bears the refusals and faults the real Slack sends.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{
    block_on_load_all, db_path_for, fetch, FetchOptions, FetchSummary, LoadedRaw, RawDb,
};
use datalib_etl_slack::probe::probe;
use datalib_etl_slack_config::{SlackApiSync, SlackConfig};
use datalib_fake_services::{Fakes, Launch, SLACK};
use datalib_probe::{ProbeAsk, ProbeList};
use serde_json::{json, Value};

const TOKEN: &str = "xoxb-fake-starfleet-token";
/// The seed's data ends here; served shifted so it ends the day before
/// the run.
const ANCHOR: &str = "2026-03-21T00:00:00Z";
/// Unix seconds of `ANCHOR`.
const ANCHOR_SECONDS: i64 = 1_774_051_200;
const DAY: i64 = 24 * 60 * 60;

/// A Slack `ts` this many seconds before the anchor.
fn ts_before(seconds: i64) -> String {
    format!("{}.000000", ANCHOR_SECONDS - seconds)
}

/// The Enterprise: three channels of which the token's user (Picard) is
/// in two, a thread on the bridge, a thread older than any refresh window
/// in engineering, and a direct message.
fn seed() -> Value {
    json!({
        "anchor": ANCHOR,
        "time_shift": "to_trial_start",
        "tokens": [{"token": TOKEN, "user": "picard"}],
        "team": {"name": "Enterprise", "domain": "enterprise"},
        "users": [
            {"name": "picard", "real_name": "Jean-Luc Picard"},
            {"name": "riker", "real_name": "William Riker"},
            {"name": "laforge", "real_name": "Geordi La Forge"},
        ],
        "channels": [
            {"name": "bridge", "members": ["picard", "riker"]},
            {"name": "engineering", "members": ["picard", "laforge"]},
            {"name": "ten-forward", "members": ["riker", "laforge"]},
        ],
        "dms": [{"key": "dm-riker", "members": ["picard", "riker"]}],
        "messages": [
            {"channel": "engineering", "user": "laforge", "ts": ts_before(60 * DAY), "text": "Warp core diagnostic scheduled."},
            {"channel": "engineering", "user": "picard", "ts": ts_before(60 * DAY - 600), "thread_ts": ts_before(60 * DAY), "text": "Keep me posted."},
            {"channel": "bridge", "user": "picard", "ts": ts_before(2 * DAY), "text": "Status report."},
            {"channel": "bridge", "user": "riker", "ts": ts_before(2 * DAY - 600), "thread_ts": ts_before(2 * DAY), "text": "All systems nominal."},
            {"channel": "bridge", "user": "riker", "ts": ts_before(DAY), "text": "Course laid in for Farpoint."},
            {"channel": "ten-forward", "user": "riker", "ts": ts_before(DAY + 3600), "text": "Poker tonight."},
            {"channel": "dm-riker", "user": "riker", "ts": ts_before(DAY + 7200), "text": "A word in private, sir?"},
        ]
    })
}

fn config() -> SlackConfig {
    SlackConfig {
        api: Some(SlackApiSync::default()),
        ..SlackConfig::default()
    }
}

/// One download into `out`, re-reading every channel's refresh window on
/// every run, with `adjust` applied on top.
async fn fetch_into(
    out: &Path,
    adjust: impl FnOnce(FetchOptions) -> FetchOptions,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(out)).await?;
    let summary = fetch(adjust(FetchOptions {
        media: false,
        refresh_every_minutes: 0,
        ..FetchOptions::new(db.clone())
    }))
    .await;
    db.commit_all("test").await?;
    db.close().await;
    summary
}

fn load(out: &Path) -> LoadedRaw {
    block_on_load_all(&db_path_for(out)).expect("load the store")
}

fn texts(raw: &LoadedRaw) -> BTreeSet<String> {
    raw.messages
        .iter()
        .map(|m| m.payload["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn text_of<'a>(raw: &'a LoadedRaw, text: &str) -> &'a Value {
    &raw.messages
        .iter()
        .find(|m| m.payload["text"] == text)
        .unwrap_or_else(|| panic!("no message reads {text:?}"))
        .payload
}

fn channel_names(raw: &LoadedRaw) -> BTreeSet<String> {
    raw.channels
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The calls of `method` made in `phase`, by their query strings.
fn calls(fakes: &Fakes, phase: &str, method: &str) -> Vec<String> {
    fakes
        .requests_in_phase(phase)
        .iter()
        .filter_map(|line| line["url"].as_str())
        .filter(|url| url.contains(&format!("/api/{method}")))
        .map(str::to_string)
        .collect()
}

/// The datalib Slack download asks for two things the fake has no answer
/// to and treats as optional: the bookmarks and the unread counts.
fn optional_calls() -> BTreeSet<String> {
    ["slack:saved.list", "slack:client.counts"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_mirrors_the_channels_the_account_is_in() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    let summary = fetch_into(out.path(), |o| o)
        .await
        .expect("the download runs");

    let raw = load(out.path());
    let shown = serde_json::to_string(&summary).unwrap();
    assert_eq!(
        channel_names(&raw),
        ["bridge", "engineering"].map(String::from).into()
    );
    assert_eq!(summary.messages, 3, "{shown}");
    assert_eq!(summary.replies, 2, "{shown}");
    assert_eq!(raw.messages.len(), 5);
    assert_eq!(
        raw.users
            .iter()
            .filter_map(|u| u["name"].as_str())
            .collect::<BTreeSet<_>>(),
        ["picard", "riker", "laforge"].into()
    );
    // Picard is not in #ten-forward, and asked for no direct messages.
    assert!(!texts(&raw).contains("Poker tonight."));
    assert!(!texts(&raw).contains("A word in private, sir?"));
    // Every call the download made is one Slack has, besides the two it
    // treats as optional.
    let unsupported: BTreeSet<String> = fakes.unsupported_calls().into_keys().collect();
    assert!(unsupported.is_subset(&optional_calls()), "{unsupported:?}");
    assert_eq!(fakes.health()["rate_limited"], json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_public_channel_is_mirrored_when_membership_is_not_required() {
    let _fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    fetch_into(out.path(), |o| FetchOptions {
        members_only: false,
        ..o
    })
    .await
    .expect("the download runs");

    let raw = load(out.path());
    assert_eq!(
        channel_names(&raw),
        ["bridge", "engineering", "ten-forward"]
            .map(String::from)
            .into()
    );
    assert!(texts(&raw).contains("Poker tonight."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_messages_are_mirrored_when_asked_for() {
    let _fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    fetch_into(out.path(), |o| FetchOptions { dms: true, ..o })
        .await
        .expect("the download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("A word in private, sir?"),
        "{:?}",
        texts(&raw)
    );
    let dm = raw
        .messages
        .iter()
        .find(|m| m.payload["text"] == "A word in private, sir?")
        .unwrap();
    assert!(dm.channel_id.starts_with('D'), "{}", dm.channel_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_names_the_account_and_lists_its_channels() {
    let _fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");

    let account = probe(&config(), ProbeAsk::Account, &|_| {})
        .await
        .expect("the probe runs");
    let channels = probe(&config(), ProbeAsk::List(ProbeList::Channels), &|_| {})
        .await
        .expect("the probe runs");
    let conversations = probe(&config(), ProbeAsk::List(ProbeList::Conversations), &|_| {})
        .await
        .expect("the probe runs");

    assert_eq!(account.mode, "api");
    assert_eq!(account.account.display_name.as_deref(), Some("picard"));
    assert_eq!(
        channels
            .items
            .iter()
            .map(|i| i.path.as_str())
            .collect::<BTreeSet<_>>(),
        ["bridge", "engineering", "ten-forward"].into()
    );
    let not_a_member: Vec<&str> = channels
        .items
        .iter()
        .filter(|i| {
            i.role
                .as_deref()
                .is_some_and(|r| r.contains("not a member"))
        })
        .map(|i| i.path.as_str())
        .collect();
    assert_eq!(not_a_member, ["ten-forward"]);
    assert_eq!(conversations.items.len(), 1, "{:?}", conversations.items);
    assert!(
        conversations.items[0]
            .title
            .as_deref()
            .is_some_and(|t| t.contains("riker")),
        "{:?}",
        conversations.items[0]
    );
}

/// What a later sync adds to the seed: a root and a reply on the bridge,
/// a reply on the old engineering thread, a new channel.
fn update() -> Value {
    json!({
        "channels": [{"name": "launch-ops", "members": ["picard", "laforge"]}],
        "messages": [
            {"channel": "bridge", "user": "riker", "ts": "first+00:05:00.000001", "text": "Farpoint station in range."},
            {"channel": "bridge", "user": "riker", "ts": "first+00:06:00.000002", "thread_ts": ts_before(2 * DAY), "text": "Shields at full."},
            {"channel": "engineering", "user": "laforge", "ts": "first+00:07:00.000003", "thread_ts": ts_before(60 * DAY), "text": "Diagnostic complete, all green."},
            {"channel": "launch-ops", "user": "laforge", "ts": "first+00:08:00.000004", "text": "Shuttle bay ready."},
        ]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_picks_up_new_roots_replies_and_channels_cheaply() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let first = fakes.requests_in_phase("first").len();
    fakes
        .add_records(SLACK, &update(), Some("first"))
        .expect("the update lands");

    fakes.mark_phase("second").unwrap();
    let second = fetch_into(out.path(), |o| FetchOptions {
        search_replies: true,
        ..o
    })
    .await
    .expect("the second download runs");

    let raw = load(out.path());
    let shown = serde_json::to_string(&second).unwrap();
    assert_eq!(
        channel_names(&raw),
        ["bridge", "engineering", "launch-ops"]
            .map(String::from)
            .into()
    );
    for text in [
        "Farpoint station in range.",
        "Shields at full.",
        "Diagnostic complete, all green.",
        "Shuttle bay ready.",
    ] {
        assert!(
            texts(&raw).contains(text),
            "{text:?} is missing from {:?}",
            texts(&raw)
        );
    }
    assert_eq!(second.messages, 2, "{shown}");
    assert_eq!(second.replies, 2, "{shown}");
    // The bridge thread's root lists a newer reply, so the thread is read
    // again; the engineering thread is older than the refresh window, so
    // its reply is found by the search, and that thread is read too.
    let replies = calls(&fakes, "second", "conversations.replies");
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert!(
        replies
            .iter()
            .any(|url| url.contains(&format!("ts={}", shifted(&fakes, 2 * DAY)))),
        "{replies:?}"
    );
    assert!(
        replies
            .iter()
            .any(|url| url.contains(&format!("ts={}", shifted(&fakes, 60 * DAY)))),
        "{replies:?}"
    );
    assert_eq!(calls(&fakes, "second", "search.messages").len(), 1);
    let second_requests = fakes.requests_in_phase("second").len();
    assert!(
        second_requests < first,
        "the second sync made {second_requests} requests, the first {first}"
    );
}

/// The seed's `ts` this many seconds before the anchor, as the fake serves
/// it under the shift it reports.
fn shifted(fakes: &Fakes, seconds_before_anchor: i64) -> String {
    let shift_seconds = fakes.health()["shift_ms"].as_i64().unwrap() / 1000;
    format!(
        "{}.000000",
        ANCHOR_SECONDS - seconds_before_anchor + shift_seconds
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_root_inside_the_refresh_window_lands_on_the_next_sync() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    fakes
        .add_records(
            SLACK,
            &json!({"messages": [
                {"channel": "bridge", "user": "riker", "ts": ts_before(DAY), "edited_ts": "first+00:01:00", "text": "Course laid in for Farpoint; ETA two days."},
            ]}),
            Some("first"),
        )
        .expect("the edit lands");

    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("Course laid in for Farpoint; ETA two days."),
        "{:?}",
        texts(&raw)
    );
    assert!(!texts(&raw).contains("Course laid in for Farpoint."));
    let edited = text_of(&raw, "Course laid in for Farpoint; ETA two days.");
    assert!(edited["edited"]["ts"].is_string(), "{edited}");
}

/// A reply edited in a thread that gained no new reply is not re-read:
/// `conversations.history` lists roots only, and the thread's
/// `latest_reply` and `reply_count` do not move for an edit, so the
/// download has nothing to notice it by (INGEST.md § "Thread replies: when
/// the thread is re-fetched anyway"). This is the gap the Imbue Studio
/// eval of 2026-11-18 surfaced; the test holds the download to what it
/// documents, and should turn around when the gap is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_reply_in_a_quiet_thread_is_not_re_read() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    fakes
        .add_records(
            SLACK,
            &json!({"messages": [
                {"channel": "bridge", "user": "riker", "ts": ts_before(2 * DAY - 600), "thread_ts": ts_before(2 * DAY), "edited_ts": "first+00:01:00", "text": "All systems nominal, except the replicators."},
            ]}),
            Some("first"),
        )
        .expect("the edit lands");

    fakes.mark_phase("second").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("All systems nominal."),
        "{:?}",
        texts(&raw)
    );
    assert!(calls(&fakes, "second", "conversations.replies").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_rate_limited_by_slack_waits_and_completes() {
    let launch = Launch {
        rate_limit_by_bucket: BTreeMap::from([("slack:conversations.history".to_string(), 1)]),
        rate_limit_window_seconds: Some(1),
    };
    let fakes =
        Fakes::start_with(&BTreeMap::from([(SLACK, seed())]), &launch).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    let summary = fetch_into(out.path(), |o| o)
        .await
        .expect("the download runs");

    assert_eq!(
        summary.messages,
        3,
        "{}",
        serde_json::to_string(&summary).unwrap()
    );
    assert_eq!(load(out.path()).messages.len(), 5);
    let refused = fakes.health()["rate_limited"]["slack:conversations.history"]
        .as_u64()
        .unwrap_or(0);
    assert!(refused >= 1, "{}", fakes.health());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_through_slacks_faults_ends_with_a_whole_mirror() {
    let mut faulted = seed();
    faulted["faults"] = json!({
        "after_requests": 2,
        "rate_limit_episode_every": 9,
        "rate_limit_episode_seconds": 1,
        "transient_error_every": 4,
        "unreadable_every": 6,
    });
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, faulted)])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    let summary = fetch_into(out.path(), |o| o)
        .await
        .expect("the download runs");

    assert_eq!(
        summary.messages,
        3,
        "{}",
        serde_json::to_string(&summary).unwrap()
    );
    assert_eq!(load(out.path()).messages.len(), 5);
    let faults = &fakes.health()["faulted"];
    let total: u64 = faults
        .as_object()
        .unwrap()
        .values()
        .filter_map(Value::as_u64)
        .sum();
    assert!(total >= 2, "{faults}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_credential_ends_the_run_and_stores_nothing() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    fakes
        .sign_in_as("slack", "Authorization: Bearer xoxb-revoked")
        .expect("the wrong token is stored");
    let out = tempfile::tempdir().unwrap();

    let err = match fetch_into(out.path(), |o| o).await {
        Ok(summary) => panic!(
            "the download ran: {}",
            serde_json::to_string(&summary).unwrap()
        ),
        Err(err) => err,
    };

    let shown = format!("{err:#}");
    assert!(shown.to_lowercase().contains("auth"), "{shown}");
    assert_eq!(load(out.path()).messages.len(), 0);
}
