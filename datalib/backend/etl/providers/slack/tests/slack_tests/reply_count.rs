//! A thread root says how many replies Slack counts and how many the
//! mirror does not hold yet, and the render after the replies arrive
//! says so no longer.

use std::collections::HashSet;
use std::path::Path;

use datalib_etl::doltlite_raw as dr;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_slack::ingest::{db_path_for, RawDb, MESSAGES_PER_PAGE};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::{checked_at, fetch_into, msg, serve, stored_ts};

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
        json!({"channel": "C1", "ts": A, "limit": MESSAGES_PER_PAGE}),
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

/// The rule in `docs/user/raw_stores.md` for whether a `ts` is mirrored,
/// its `:channel` and `:ts` written `?1` and `?2`.
const IS_MIRRORED: &str = "\
    WITH q(scope, ts, key) AS (
      SELECT 'history:' || ?1, ?2, substr('0000000000000000000' || ?2, -19))
    SELECT EXISTS (SELECT 1 FROM coverage c, q
                    WHERE c.scope = q.scope AND c.lo <= q.key AND q.key <= c.hi)
        OR EXISTS (SELECT 1 FROM coverage_checked k, q
                    WHERE k.scope = q.scope
                      AND CAST(q.ts AS REAL) <= unixepoch(k.checked_at_utc)
                      AND q.key > COALESCE(
                            (SELECT max(hi) FROM coverage WHERE scope = q.scope), ''))";

/// #1606: a reply newer than a channel's last top-level message lies past
/// `hi`, which is the newest top-level message the walk saw. Before, the
/// guide sent an app to Slack for it, since nothing said when the walk
/// was. Now the store says the walk reached the present, and only what is
/// newer than that walk is not mirrored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_past_the_newest_top_level_message_counts_as_mirrored() {
    let d = tempdir().unwrap();
    let out = d.path().join("out_raw");
    let api = d.path().join("api");
    record_run(
        &api,
        History::cold("C1"),
        json!([root()]),
        json!({"ok": true, "has_more": false,
               "messages": [root(), msg(REPLY_1, "aye"), msg(REPLY_2, "make it so")]}),
    );
    let walked_at = chrono::DateTime::from_timestamp(1735693200, 0).unwrap();
    serve(&api, &d.path().join("playback"));
    fetch_into(&d.path().join("playback"), &out, |o| {
        datalib_etl_slack::ingest::FetchOptions {
            now: walked_at,
            ..o
        }
    })
    .await
    .unwrap();
    assert_eq!(stored_ts(&out), [A, REPLY_1, REPLY_2]);
    assert_eq!(checked_at(&out).await["C1"], walked_at);

    let reader = datalib_pin::open_reader(&db_path_for(&out)).await.unwrap();
    let hi: String = sqlx::query_scalar("SELECT hi FROM coverage WHERE scope = 'history:C1'")
        .fetch_one(&reader)
        .await
        .unwrap();
    assert!(
        hi.as_str() < format!("00{REPLY_2}").as_str(),
        "the reply lies past the span: {hi}"
    );
    let mirrored = |ts: &'static str| {
        let reader = reader.clone();
        async move {
            sqlx::query_scalar::<_, bool>(IS_MIRRORED)
                .bind("C1")
                .bind(ts)
                .fetch_one(&reader)
                .await
                .unwrap()
        }
    };
    assert!(mirrored(A).await, "inside the span");
    assert!(mirrored(REPLY_2).await, "past the span, before the walk");
    assert!(!mirrored("1735693201.000000").await, "after the walk");
    reader.close().await;
}
