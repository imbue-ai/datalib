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
use datalib_fake_services::{with_real_rate_limits, Fakes, Launch, SLACK};
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
        "rate_limits": "none",
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

/// How many conversations the store holds a message of.
fn walked_channels(raw: &LoadedRaw) -> usize {
    raw.messages
        .iter()
        .map(|m| m.channel_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
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
    // The directory lists every channel; only the two Picard is in are walked.
    assert_eq!(
        channel_names(&raw),
        ["bridge", "engineering", "ten-forward"]
            .map(String::from)
            .into()
    );
    assert_eq!(walked_channels(&raw), 2);
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
    assert!(texts(&raw).contains("Poker tonight."), "{:?}", texts(&raw));
    assert_eq!(walked_channels(&raw), 3);
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
    assert_eq!(
        account.account.display_name.as_deref(),
        Some("picard in Enterprise")
    );
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
            .is_some_and(|t| t.contains("William Riker")),
        "{:?}",
        conversations.items[0]
    );
}

/// What a later sync adds to the seed: a root and a reply on the bridge,
/// a reply on the old engineering thread, a new channel. Its instants sit
/// seconds past the first sync's last request, so the fake takes them
/// after a short wait where a trial would sleep out minutes.
fn update() -> Value {
    json!({
        "channels": [{"name": "launch-ops", "members": ["picard", "laforge"]}],
        "messages": [
            {"channel": "bridge", "user": "riker", "ts": "first+00:00:01.000001", "text": "Farpoint station in range."},
            {"channel": "bridge", "user": "riker", "ts": "first+00:00:02.000002", "thread_ts": ts_before(2 * DAY), "text": "Shields at full."},
            {"channel": "engineering", "user": "laforge", "ts": "first+00:00:03.000003", "thread_ts": ts_before(60 * DAY), "text": "Diagnostic complete, all green."},
            {"channel": "launch-ops", "user": "laforge", "ts": "first+00:00:04.000004", "text": "Shuttle bay ready."},
        ]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_picks_up_new_roots_replies_and_channels_and_re_reads_only_the_threads_that_moved(
) {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let before = load(out.path());
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
    assert!(
        channel_names(&raw).contains("launch-ops"),
        "{:?}",
        channel_names(&raw)
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
    // The summary counts what the run read, re-read window included: the
    // four new messages are what the store gained.
    assert_eq!(texts(&raw).len(), texts(&before).len() + 4, "{shown}");
    assert!(second.messages >= 2 && second.replies >= 2, "{shown}");
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
                {"channel": "bridge", "user": "riker", "ts": ts_before(DAY), "edited_ts": "first+00:00:01", "text": "Course laid in for Farpoint; ETA two days."},
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
                {"channel": "bridge", "user": "riker", "ts": ts_before(2 * DAY - 600), "thread_ts": ts_before(2 * DAY), "edited_ts": "first+00:00:01", "text": "All systems nominal, except the replicators."},
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
    // One history call a window: the second channel's is refused however
    // slowly the download runs, and waits out the rest of the window.
    let launch = Launch {
        rate_limit_by_bucket: BTreeMap::from([("slack:conversations.history".to_string(), 1)]),
        rate_limit_window_seconds: Some(5),
    };
    let fakes = Fakes::start_with(
        &BTreeMap::from([(SLACK, with_real_rate_limits(seed()))]),
        &launch,
    )
    .expect("the fakes serve");
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

// What a later sync has to notice when the workspace changes under it:
// driven through the fake's own Slack API as Picard, the way a person
// using Slack would make the change.

const AUTH: &str = "Authorization: Bearer xoxb-fake-starfleet-token";

/// A Slack method called as Picard, which must answer `ok`.
fn slack_do(fakes: &Fakes, method: &str, form: &[(&str, &str)]) -> Value {
    let answer = fakes
        .post_form(SLACK, &format!("/api/{method}"), AUTH, form)
        .expect("the fake answers");
    assert_eq!(answer["ok"], json!(true), "{method}: {answer}");
    answer
}

/// The id Slack gives the conversation named `name`, as the download's
/// store holds it.
fn channel_id(raw: &LoadedRaw, name: &str) -> String {
    raw.channels
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no channel {name}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The stored `ts` of the message reading `text`, which under the shift
/// only the store knows.
fn ts_of(raw: &LoadedRaw, text: &str) -> String {
    raw.messages
        .iter()
        .find(|m| m.payload["text"] == text)
        .unwrap_or_else(|| panic!("no message reads {text:?}"))
        .ts
        .clone()
}

/// The `conversations.history` calls of `phase` for the conversation `id`.
fn history_calls(fakes: &Fakes, phase: &str, id: &str) -> usize {
    calls(fakes, phase, "conversations.history")
        .iter()
        .filter(|url| url.contains(&format!("channel={id}")))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_deleted_upstream_inside_the_window_is_pruned() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let raw = load(out.path());
    let bridge = channel_id(&raw, "bridge");
    // Slack lets a person delete their own messages: Picard posts, syncs, deletes.
    slack_do(
        &fakes,
        "chat.postMessage",
        &[("channel", &bridge), ("text", "Make it so.")],
    );
    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");
    let raw = load(out.path());
    assert!(texts(&raw).contains("Make it so."));
    slack_do(
        &fakes,
        "chat.delete",
        &[("channel", &bridge), ("ts", &ts_of(&raw, "Make it so."))],
    );

    let third = fetch_into(out.path(), |o| o)
        .await
        .expect("the third download runs");

    let raw = load(out.path());
    assert!(!texts(&raw).contains("Make it so."), "{:?}", texts(&raw));
    assert_eq!(
        third.pruned,
        1,
        "{}",
        serde_json::to_string(&third).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_deleted_upstream_goes_when_its_thread_is_read_again() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let raw = load(out.path());
    let bridge = channel_id(&raw, "bridge");
    let root = ts_of(&raw, "Status report.");
    // Slack lets a person delete their own messages: Picard replies, syncs,
    // deletes the reply, and the thread gains another so it is read again.
    slack_do(
        &fakes,
        "chat.postMessage",
        &[
            ("channel", &bridge),
            ("thread_ts", &root),
            ("text", "Acknowledged."),
        ],
    );
    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");
    let raw = load(out.path());
    assert!(texts(&raw).contains("Acknowledged."));
    slack_do(
        &fakes,
        "chat.delete",
        &[("channel", &bridge), ("ts", &ts_of(&raw, "Acknowledged."))],
    );
    slack_do(
        &fakes,
        "chat.postMessage",
        &[
            ("channel", &bridge),
            ("thread_ts", &root),
            ("text", "Shields holding."),
        ],
    );

    fetch_into(out.path(), |o| o)
        .await
        .expect("the third download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("Shields holding."),
        "{:?}",
        texts(&raw)
    );
    assert!(!texts(&raw).contains("Acknowledged."), "{:?}", texts(&raw));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reaction_added_inside_the_window_lands_on_the_next_sync() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let raw = load(out.path());
    slack_do(
        &fakes,
        "reactions.add",
        &[
            ("channel", &channel_id(&raw, "bridge")),
            ("timestamp", &ts_of(&raw, "Course laid in for Farpoint.")),
            ("name", "rocket"),
        ],
    );

    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");

    let raw = load(out.path());
    let reactions = &text_of(&raw, "Course laid in for Farpoint.")["reactions"];
    assert_eq!(reactions[0]["name"], "rocket", "{reactions}");
}

/// Picard is not in #ten-forward, whose one message is older than the
/// refresh window. Once he joins, the next sync walks the channel from the
/// start, not from the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_the_account_joins_later_is_walked_from_its_start() {
    let mut seeded = seed();
    seeded["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"channel": "ten-forward", "user": "laforge", "ts": ts_before(45 * DAY), "text": "Welcome aboard, Ensign."}));
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seeded)])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    let raw = load(out.path());
    assert!(!texts(&raw).contains("Poker tonight."));
    slack_do(
        &fakes,
        "conversations.join",
        &[("channel", &channel_id(&raw, "ten-forward"))],
    );

    fetch_into(out.path(), |o| o)
        .await
        .expect("the second download runs");

    let raw = load(out.path());
    assert!(texts(&raw).contains("Poker tonight."), "{:?}", texts(&raw));
    assert!(
        texts(&raw).contains("Welcome aboard, Ensign."),
        "{:?}",
        texts(&raw)
    );
}

/// An archived channel is read once (INGEST.md § "Archived channels"): the
/// run that finds it archived walks it whole and leaves it alone after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archived_channel_is_walked_once_and_then_left_alone() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| FetchOptions {
        archived: true,
        ..o
    })
    .await
    .expect("the first download runs");
    let raw = load(out.path());
    let engineering = channel_id(&raw, "engineering");
    slack_do(
        &fakes,
        "conversations.archive",
        &[("channel", &engineering)],
    );

    fakes.mark_phase("second").unwrap();
    fetch_into(out.path(), |o| FetchOptions {
        archived: true,
        ..o
    })
    .await
    .expect("the second download runs");
    fakes.mark_phase("third").unwrap();
    fetch_into(out.path(), |o| FetchOptions {
        archived: true,
        ..o
    })
    .await
    .expect("the third download runs");

    let raw = load(out.path());
    assert!(texts(&raw).contains("Warp core diagnostic scheduled."));
    assert_eq!(
        raw.channels
            .iter()
            .find(|c| c["name"] == "engineering")
            .unwrap()["is_archived"],
        json!(true)
    );
    assert!(history_calls(&fakes, "second", &engineering) >= 1);
    assert_eq!(history_calls(&fakes, "third", &engineering), 0);
}

/// Slack takes at most twenty `in:` filters in one search, so a reply
/// search over more channels than that is more than one query, and every
/// old thread's new reply is still found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_search_over_more_channels_than_one_query_names_finds_every_reply() {
    let mut seeded = seed();
    let mut replies = Vec::new();
    for n in 0..25 {
        let name = format!("deck-{n:02}");
        seeded["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": name, "members": ["picard", "riker"]}));
        seeded["messages"].as_array_mut().unwrap().push(json!({
            "channel": name, "user": "riker", "ts": ts_before(50 * DAY + n * 60), "text": format!("Deck {n} report.")
        }));
        replies.push(json!({
            "channel": name, "user": "riker", "ts": format!("first+00:00:01.{n:06}"),
            "thread_ts": ts_before(50 * DAY + n * 60), "text": format!("Deck {n} follow-up.")
        }));
    }
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seeded)])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(out.path(), |o| o)
        .await
        .expect("the first download runs");
    fakes
        .add_records(SLACK, &json!({"messages": replies}), Some("first"))
        .expect("the update lands");

    fakes.mark_phase("second").unwrap();
    fetch_into(out.path(), |o| FetchOptions {
        search_replies: true,
        ..o
    })
    .await
    .expect("the second download runs");

    let raw = load(out.path());
    let missing: Vec<String> = (0..25)
        .map(|n| format!("Deck {n} follow-up."))
        .filter(|t| !texts(&raw).contains(t))
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
    assert!(calls(&fakes, "second", "search.messages").len() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_message_opened_after_the_first_sync_is_mirrored_on_the_next() {
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seed())])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    let dms = |o: FetchOptions| FetchOptions {
        dms: true,
        list_every_hours: 0,
        ..o
    };
    fetch_into(out.path(), dms)
        .await
        .expect("the first download runs");
    let raw = load(out.path());
    let user_id = |name: &str| -> String {
        raw.users.iter().find(|u| u["name"] == name).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // A direct message has no name; Riker's is the im whose other party he is.
    let riker_dm = raw
        .channels
        .iter()
        .find(|c| c["is_im"] == json!(true) && c["user"] == user_id("riker"))
        .expect("Riker's im is listed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let opened = slack_do(
        &fakes,
        "conversations.open",
        &[("users", &user_id("laforge"))],
    );
    let laforge_dm = opened["channel"]["id"].as_str().unwrap().to_string();
    slack_do(
        &fakes,
        "chat.postMessage",
        &[
            ("channel", &laforge_dm),
            ("text", "Geordi, a word about the warp core."),
        ],
    );
    slack_do(
        &fakes,
        "chat.postMessage",
        &[
            ("channel", &riker_dm),
            ("text", "Number One, my ready room."),
        ],
    );

    fetch_into(out.path(), dms)
        .await
        .expect("the second download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("Number One, my ready room."),
        "{:?}",
        texts(&raw)
    );
    assert!(
        texts(&raw).contains("Geordi, a word about the warp core."),
        "{:?}",
        texts(&raw)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_private_channel_the_account_is_in_is_mirrored() {
    let mut seeded = seed();
    seeded["channels"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "senior-staff", "is_private": true, "members": ["picard", "riker"]}));
    seeded["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"channel": "senior-staff", "user": "riker", "ts": ts_before(DAY + 600), "text": "Eyes only: the Borg."}));
    let _fakes = Fakes::start(&BTreeMap::from([(SLACK, seeded)])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();

    fetch_into(out.path(), |o| o)
        .await
        .expect("the download runs");

    let raw = load(out.path());
    assert!(
        texts(&raw).contains("Eyes only: the Borg."),
        "{:?}",
        texts(&raw)
    );
    assert_eq!(
        raw.channels
            .iter()
            .find(|c| c["name"] == "senior-staff")
            .unwrap()["is_private"],
        json!(true)
    );
}

/// A channel with more roots than a history page is mirrored whole, and
/// the window re-read pages too; a thread of more replies than a page is
/// mirrored whole as well, though the fake serves a thread's replies in one
/// page, so only the download's paging of history is exercised here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_a_page_of_roots_and_of_replies_is_read_whole() {
    let mut seeded = seed();
    seeded["channels"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "ops-log", "members": ["picard", "laforge"]}));
    let messages = seeded["messages"].as_array_mut().unwrap();
    for n in 0..1200 {
        messages.push(json!({
            "channel": "ops-log", "user": "laforge", "ts": ts_before(20 * DAY - n * 60), "text": format!("Log entry {n}.")
        }));
    }
    for n in 0..1100 {
        messages.push(json!({
            "channel": "bridge", "user": "riker", "ts": ts_before(2 * DAY - 1200 - n * 10), "thread_ts": ts_before(2 * DAY),
            "text": format!("Nominal, check {n}.")
        }));
    }
    let fakes = Fakes::start(&BTreeMap::from([(SLACK, seeded)])).expect("the fakes serve");
    let out = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();

    let summary = fetch_into(out.path(), |o| o)
        .await
        .expect("the download runs");

    let raw = load(out.path());
    let shown = serde_json::to_string(&summary).unwrap();
    assert_eq!(summary.messages, 3 + 1200, "{shown}");
    assert_eq!(summary.replies, 2 + 1100, "{shown}");
    assert_eq!(raw.messages.len(), 5 + 1200 + 1100);
    assert!(history_calls(&fakes, "first", &channel_id(&raw, "ops-log")) >= 2);
}
