//! `client.counts` and `saved.list` answer only some tokens. One that is
//! refused them was asked both every sync, and each refusal was a
//! warning (an error, through a latchkey gateway) on the Manage row of a
//! mirror missing nothing else. A refusal is now remembered for a week:
//! the call is not asked meanwhile, and stands as one `info` row.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_slack::ingest::{
    db_path_for, fetch, FetchOptions, RawDb, MESSAGES_PER_PAGE, REFUSAL_REMEMBERED_FOR,
};
use datalib_etl_slack::recorded::{
    record_auth, record_call, record_conversations, record_users, History, CHANNEL_TYPES,
};
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::playback::{self, Asked, Playback};
use datalib_etl_web::synthesize::write_fixture;
use serde_json::{json, Value};

use crate::support::{msg, serve, stored_ts};

const A: &str = "1735689600.000100";

/// The first run's `now`; a later run is so many days after it.
const DAY_0: &str = "2025-01-02T00:00:00Z";

fn day(n: i64) -> DateTime<Utc> {
    DAY_0.parse::<DateTime<Utc>>().unwrap() + Duration::days(n)
}

/// One of the two calls, and how a tape answers it.
struct Call {
    method: &'static str,
    /// The first request the call makes: the one a refusal answers.
    first_url: &'static str,
    record_ok: fn(&Path),
    record_first: fn(&Path, Value),
}

const COUNTS: Call = Call {
    method: "client.counts",
    first_url: "https://slack.com/api/client.counts",
    record_ok: |api| {
        record_call(
            api,
            "client.counts",
            json!({}),
            json!({"ok": true, "channels": [], "mpims": [], "ims": []}),
        )
        .unwrap()
    },
    record_first: |api, answer| record_call(api, "client.counts", json!({}), answer).unwrap(),
};

const SAVED: Call = Call {
    method: "saved.list",
    first_url: "https://slack.com/api/saved.list?filter=saved&limit=50",
    record_ok: |api| {
        for filter in ["saved", "completed", "archived"] {
            record_call(
                api,
                "saved.list",
                json!({"filter": filter, "limit": "50"}),
                json!({"ok": true, "saved_items": []}),
            )
            .unwrap();
        }
    },
    record_first: |api, answer| {
        record_call(
            api,
            "saved.list",
            json!({"filter": "saved", "limit": "50"}),
            answer,
        )
        .unwrap()
    },
};

/// How the tape answers the call under test this run.
enum Answer {
    Ok,
    /// Slack's `ok: false` with this `error`.
    Slack(&'static str),
    /// This status and body, as a gateway or a failing server sends it.
    Http(u16, &'static str),
    /// Not on the tape: a run that asks fails the call, and says so.
    Unrecorded,
}

/// `#bridge`, holding one message, with the other optional call
/// answering and `call` answering `answer`.
fn record_run(api: &Path, call: &Call, answer: &Answer) {
    record_auth(api).unwrap();
    record_users(api, json!([{"id": "U1", "name": "picard"}])).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true}]),
    )
    .unwrap();
    History::cold("C1")
        .record(api, json!([msg(A, "status report")]))
        .unwrap();
    record_call(
        api,
        "conversations.history",
        json!({"channel": "C1", "include_all_metadata": "true", "inclusive": "false",
               "limit": MESSAGES_PER_PAGE, "oldest": A}),
        json!({"ok": true, "messages": [], "has_more": false}),
    )
    .unwrap();
    let other = if call.method == COUNTS.method {
        &SAVED
    } else {
        &COUNTS
    };
    (other.record_ok)(api);
    match answer {
        Answer::Ok => (call.record_ok)(api),
        Answer::Slack(error) => (call.record_first)(api, json!({"ok": false, "error": error})),
        Answer::Http(..) | Answer::Unrecorded => {}
    }
}

/// One sync on `day` with `call` answering `answer`. Returns what was
/// asked.
async fn sync(out: &Path, on: i64, call: &Call, answer: Answer) -> Asked {
    let scratch = tempfile::tempdir().unwrap();
    let (api, tape) = (scratch.path().join("api"), scratch.path().join("tape"));
    record_run(&api, call, &answer);
    serve(&api, &tape);
    if let Answer::Http(status, body) = answer {
        let answer = HttpResponse {
            status,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        };
        let req = HttpRequest::get(HttpService::Slack, call.first_url);
        write_fixture(&tape, &req, &answer).unwrap();
    }

    let asked = Asked::default();
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let download = fetch(FetchOptions {
        refresh_window_days: 0,
        members_only: false,
        media: false,
        now: day(on),
        ..FetchOptions::new(db.clone())
    });
    let ran = playback::scope(Playback::at(&tape).asked(asked.clone()), download).await;
    if ran.is_ok() {
        db.commit_all("test").await.unwrap();
    }
    db.close().await;
    ran.unwrap();
    asked
}

/// `(scope_key, severity, sample)` of every `problems` row.
async fn problems(out: &Path) -> Vec<(String, String, String)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows =
        sqlx::query_as("SELECT scope_key, severity, sample FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;
    rows
}

async fn remembered(out: &Path, call: &Call) -> bool {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_scope_state WHERE scope = ?")
        .bind(format!("slack:sweep:refused:{}", call.method))
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await;
    n > 0
}

/// The one row a remembered refusal is: `info`, keyed as the listing,
/// saying when it is asked again.
fn assert_one_info_row(rows: &[(String, String, String)], call: &Call, again: &str) {
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (key, severity, sample) = &rows[0];
    assert_eq!(key, &format!("listing:{}", call.method));
    assert_eq!(severity, "info", "{sample}");
    assert!(
        sample.contains(&format!("not asked again until {again}")),
        "{sample}"
    );
}

/// Refused on the first sync: asked once, one `info` row and nothing
/// louder. Not asked the next day, the row unchanged and the rest of
/// the mirror synced as before. Asked again once the refusal is a week
/// old, and an answer clears the row and the marker.
async fn a_refusal_is_remembered_for_a_week(call: &Call, refusal: Answer) {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("out_raw");

    let asked = sync(&out, 0, call, refusal).await;
    assert_eq!(asked.count(call.method), 1);
    let first = problems(&out).await;
    assert_one_info_row(&first, call, "2025-01-09");
    assert!(remembered(&out, call).await);
    assert_eq!(stored_ts(&out), [A]);

    let asked = sync(&out, 1, call, Answer::Unrecorded).await;
    assert_eq!(asked.count(call.method), 0, "asked while refused");
    assert!(asked.count("conversations.history") > 0, "the walk went on");
    assert_eq!(problems(&out).await, first, "the same row, word for word");

    let past = REFUSAL_REMEMBERED_FOR.num_days() + 1;
    let asked = sync(&out, past, call, Answer::Ok).await;
    assert!(
        asked.count(call.method) >= 1,
        "not asked once the week was up"
    );
    assert_eq!(problems(&out).await, []);
    assert!(!remembered(&out, call).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_counts_slack_refuses_is_asked_once_a_week() {
    a_refusal_is_remembered_for_a_week(&COUNTS, Answer::Slack("unknown_method")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_list_slack_refuses_is_asked_once_a_week() {
    a_refusal_is_remembered_for_a_week(&SAVED, Answer::Slack("not_allowed_token_type")).await;
}

/// A latchkey gateway's own 403 was an error row every sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_a_latchkey_gateway_refuses_is_asked_once_a_week() {
    let gateway = r#"{"error":"Error: Request not permitted.","latchkeyError":true}"#;
    a_refusal_is_remembered_for_a_week(&COUNTS, Answer::Http(403, gateway)).await;
    a_refusal_is_remembered_for_a_week(&SAVED, Answer::Http(403, gateway)).await;
}

/// Each answer that says the token may not make the call is remembered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_kind_of_refusal_is_remembered() {
    for code in ["unknown_method", "not_allowed_token_type", "missing_scope"] {
        for call in [&COUNTS, &SAVED] {
            let d = tempfile::tempdir().unwrap();
            let out = d.path().join("out_raw");
            sync(&out, 0, call, Answer::Slack(code)).await;
            assert!(remembered(&out, call).await, "{} {code}", call.method);
            assert_one_info_row(&problems(&out).await, call, "2025-01-09");
        }
    }
}

/// A failure that may pass is an error row, as before, and the next
/// sync asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_failure_is_not_remembered() {
    for call in [&COUNTS, &SAVED] {
        for failure in [
            Answer::Http(500, "upstream connect error"),
            Answer::Slack("internal_error"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let out = d.path().join("out_raw");
            sync(&out, 0, call, failure).await;
            assert!(!remembered(&out, call).await, "{}", call.method);
            let rows = problems(&out).await;
            assert_eq!(
                rows.iter()
                    .map(|(k, s, _)| (k.as_str(), s.as_str()))
                    .collect::<Vec<_>>(),
                [(format!("listing:{}", call.method).as_str(), "error")],
            );

            let asked = sync(&out, 1, call, Answer::Ok).await;
            assert!(asked.count(call.method) >= 1, "{}", call.method);
            assert_eq!(problems(&out).await, []);
        }
    }
}
