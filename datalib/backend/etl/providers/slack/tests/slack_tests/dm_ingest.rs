//! Whole-download tests for the `dms` / `dm_conversations` config knobs.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::{
    record_auth, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};

use crate::support::{channels_with_messages, fetch_into, msg, Tree};

const DM_TYPES: &str = "public_channel,private_channel,im,mpim";

fn write_auth_and_users(api: &Path) {
    record_auth(api).unwrap();
    record_users(
        api,
        json!([
            {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
            {"id": "U2", "name": "riker", "real_name": "William Riker"},
            {"id": "U3", "name": "data", "real_name": "Data"},
        ]),
    )
    .unwrap();
}

fn write_channels_only(api: &Path) {
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    )
    .unwrap();
}

fn all_conversations() -> Value {
    json!([
        {"id": "C1", "name": "bridge", "is_channel": true, "is_member": true,
         "is_archived": false},
        // A 1:1 DM: no `name`, and — the field that would otherwise
        // filter every DM out — no `is_member`.
        {"id": "D1", "is_im": true, "user": "U2", "is_archived": false,
         "is_user_deleted": false},
        {"id": "D2", "is_im": true, "user": "U3", "is_archived": false,
         "is_user_deleted": false},
        // A group DM: a private channel that also lists its members,
        // the account included.
        {"id": "G1", "is_mpim": true, "is_channel": true, "is_private": true,
         "is_member": true, "is_archived": false,
         "name": "mpdm-picard--riker--data-1", "members": ["U1", "U2", "U3"]},
    ])
}

/// History for every conversation above. Serving all four in every
/// scenario is what makes the narrowing assertions real: a run that
/// wrongly walks a DM finds a fixture waiting and stores its message,
/// so the assertion fails instead of a missing fixture producing a
/// swallowed per-channel warning that looks like a correct skip.
fn write_all_histories(api: &Path) {
    for (channel, ts, text) in [
        ("C1", "1735689600.000100", "in the channel"),
        ("D1", "1735689600.000200", "dm with riker"),
        ("D2", "1735689600.000300", "dm with data"),
        ("G1", "1735689600.000400", "group dm"),
    ] {
        History::cold(channel)
            .record(api, json!([msg(ts, text)]))
            .unwrap();
    }
}

async fn run_fetch(out: &Path, dms: bool, dm_conversations: Option<Vec<&str>>) {
    fetch_into(out, |o| FetchOptions {
        dms,
        dm_conversations: dm_conversations.map(|v| v.into_iter().map(String::from).collect()),
        ..o
    })
    .await
    .unwrap();
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// Backward compatibility, and the default every existing config gets:
/// DMs off means the request never asks for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dms_off_never_asks_for_direct_messages() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    write_channels_only(&t.api);
    write_all_histories(&t.api);

    t.serve();

    run_fetch(&t.out, false, None).await;

    assert_eq!(channels_with_messages(&t.out), set(&["C1"]));
}

/// The headline behavior: `dms = true` lists and walks both DM shapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dms_on_mirrors_direct_and_group_messages() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    record_conversations(&t.api, DM_TYPES, all_conversations()).unwrap();
    write_all_histories(&t.api);

    t.serve();

    run_fetch(&t.out, true, None).await;

    assert_eq!(
        channels_with_messages(&t.out),
        set(&["C1", "D1", "D2", "G1"]),
        "dms = true should mirror the channel, both 1:1 DMs and the group DM",
    );
}

/// `dm_conversations` narrows to exactly the conversations named. Every
/// other DM has a fixture ready, so walking one lands its message and
/// fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dm_conversations_narrows_to_the_named_conversations() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    record_conversations(&t.api, DM_TYPES, all_conversations()).unwrap();
    write_all_histories(&t.api);

    t.serve();

    // One as a bare id, one as the link `Copy link` hands out.
    run_fetch(
        &t.out,
        true,
        Some(vec!["D1", "https://enterprise.slack.com/archives/G1"]),
    )
    .await;

    assert_eq!(
        channels_with_messages(&t.out),
        set(&["C1", "D1", "G1"]),
        "Riker's 1:1 DM and the group DM were named; Data's 1:1 (D2) was \
         not and must be left alone",
    );
}

/// A list that names nothing this account has must mirror no DMs — not
/// fall open to all of them, and not quietly turn a user id or a name
/// into a match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dm_conversations_naming_nothing_walks_no_dms() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    record_conversations(&t.api, DM_TYPES, all_conversations()).unwrap();
    write_all_histories(&t.api);

    t.serve();

    // A person, not a conversation — the shape the old `dm_users` took.
    run_fetch(&t.out, true, Some(vec!["U2", "@riker"])).await;

    assert_eq!(channels_with_messages(&t.out), set(&["C1"]));
}

/// Turning DMs on for an already-synced store has to refetch
/// `conversations.list`, even inside the six-hour sweep TTL — the
/// cached listing predates the wider `types` and contains no DM rows,
/// so honoring it would mirror nothing and report success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_dms_on_relists_despite_the_sweep_ttl() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    write_channels_only(&t.api);
    record_conversations(&t.api, DM_TYPES, all_conversations()).unwrap();
    write_all_histories(&t.api);

    t.serve();

    run_fetch(&t.out, false, None).await;
    assert_eq!(channels_with_messages(&t.out), set(&["C1"]));

    // Run 2, seconds later — well inside MANIFEST_TTL.
    run_fetch(&t.out, true, None).await;
    assert_eq!(
        channels_with_messages(&t.out),
        set(&["C1", "D1", "D2", "G1"]),
        "the second run must re-list under the wider `types` rather than \
         serve the cached channels-only sweep",
    );
}

/// Turning DMs back off stops walking them, and — like every other
/// narrowing in this provider — leaves what is already mirrored alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_dms_off_stops_walking_them_without_deleting() {
    let t = Tree::new();
    write_auth_and_users(&t.api);
    record_conversations(&t.api, DM_TYPES, all_conversations()).unwrap();
    write_channels_only(&t.api);
    write_all_histories(&t.api);
    // Run 2 resumes C1 at its resume cursor (exclusive), which is a
    // different param set and so needs its own fixture.
    History {
        inclusive: false,
        ..History::from("C1", "1735689600.000100")
    }
    .record(&t.api, json!([]))
    .unwrap();

    t.serve();

    run_fetch(&t.out, true, None).await;
    assert_eq!(
        channels_with_messages(&t.out),
        set(&["C1", "D1", "D2", "G1"])
    );

    // The DM history fixtures are still served, so a run that kept
    // walking them would succeed — the assertion is that it doesn't
    // need to, and that nothing is dropped either.
    run_fetch(&t.out, false, None).await;
    assert_eq!(
        channels_with_messages(&t.out),
        set(&["C1", "D1", "D2", "G1"]),
        "narrowing must not delete already-mirrored DMs",
    );
}
