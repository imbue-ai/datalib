//! A thread root says how many replies Slack counts and how many the
//! mirror does not hold yet, and the render after the replies arrive
//! says so no longer.

use std::collections::HashSet;
use std::path::Path;

use datalib_etl::doltlite_raw as dr;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_slack::ingest::{db_path_for, RawDb};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{fetch_into, msg, serve};

const A: &str = "1735689600.000100";
const REPLY_1: &str = "1735689600.000150";
const REPLY_2: &str = "1735689600.000160";
const B: &str = "1735689600.000200";

fn root() -> Value {
    json!({"ts": A, "user": "U1", "text": "status report", "thread_ts": A,
           "reply_count": 2, "latest_reply": REPLY_2})
}

/// `#bridge` with the thread rooted at `A`, its replies answered by
/// `replies`. The account-state listings answer empty.
fn record_run(api: &Path, history: History<'_>, messages: Value, replies: Value) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    )
    .unwrap();
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
    history.record(api, messages).unwrap();
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": A, "limit": "200"}),
        replies,
    )
    .unwrap();
}

async fn sync(out: &Path, api: &Path, playback: &Path) -> String {
    serve(api, playback);
    fetch_into(playback, out, |o| o).await.unwrap();
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let head = dr::head_commit(db.pool()).await.unwrap().expect("a commit");
    db.close().await;
    head
}

/// The threads the render was narrowed to, and the markdown of the
/// "status report" thread if it rendered.
fn render(raw: &Path, range: RawRange<'_>) -> (Option<HashSet<String>>, Option<String>) {
    let store = datalib_etl_slack_render::render::open(raw, "src", range)
        .unwrap()
        .expect("a committed store");
    let out = tempdir().unwrap();
    let mut thread = None;
    datalib_etl_slack_render::render::render::render_store(
        &store,
        out.path(),
        "src",
        datalib_etl_render::pages::PageSize {
            buckets: 1,
            bytes: 1,
        },
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            let md: String = doc.sections.iter().map(|s| s.md.as_str()).collect();
            if md.contains("title: \"#bridge: status report") {
                thread = Some(md);
            }
            Ok(())
        },
        &mut |_| Ok(()),
    )
    .unwrap();
    let narrowed = store.scan.render.clone();
    store.close();
    (narrowed, thread)
}

fn replies_line(md: &str) -> Option<&str> {
    md.lines().find(|l| l.contains("msg-replies"))
}

/// A first sync whose thread read failed holds the root and none of its
/// replies: the root says both are still to come. The next sync reads
/// the thread, and the incremental render names the thread, whose root
/// now says only how many replies it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_says_its_replies_are_not_mirrored_until_they_are() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");

    let api1 = d.path().join("api1");
    record_run(
        &api1,
        History::cold("C1"),
        json!([msg(B, "second"), root()]),
        json!({"ok": false, "error": "internal_error"}),
    );
    let first = sync(&out, &api1, &d.path().join("playback1")).await;

    let (_, cold) = render(&out, RawRange::cold());
    let cold = cold.expect("the thread rendered");
    assert_eq!(
        replies_line(&cold),
        Some("<span class=\"msg-replies\">2 replies · 2 not mirrored yet</span>"),
        "{cold}"
    );

    let api2 = d.path().join("api2");
    record_run(
        &api2,
        History {
            inclusive: false,
            ..History::from("C1", B)
        },
        json!([]),
        json!({"ok": true, "has_more": false, "messages": [
            root(),
            {"ts": REPLY_1, "user": "U1", "text": "all nominal", "thread_ts": A},
            {"ts": REPLY_2, "user": "U1", "text": "warp five", "thread_ts": A},
        ]}),
    );
    let second = sync(&out, &api2, &d.path().join("playback2")).await;

    let nothing_stale = HashSet::new();
    let (narrowed, incremental) = render(
        &out,
        RawRange {
            cursor: Some(&first),
            pin: Some(&second),
            stale: Some(&nothing_stale),
        },
    );
    let named = narrowed.expect("a narrowed render");
    assert!(named.contains(&format!("T1#C1#{A}")), "{named:?}");
    let incremental = incremental.expect("the thread rendered again");
    assert_eq!(
        replies_line(&incremental),
        Some("<span class=\"msg-replies\">2 replies</span>"),
        "{incremental}"
    );
}
