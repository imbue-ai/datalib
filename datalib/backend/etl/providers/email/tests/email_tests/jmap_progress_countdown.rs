//! The Fastmail (JMAP) half of what `progress_countdown` asserts for
//! Gmail: the download announces a total big enough to count down from,
//! and every phase's ticks add up to it.
//!
//! This path used to announce `5` — its phase count — so a mailbox of
//! forty thousand messages counted down from four. `Email/query` is
//! already sent `calculateTotal: true` and its `total` was read only to
//! decide when to stop paginating; the enumeration now announces it.
//!
//! Fixtures are keyed on the request's exact bytes, so the envelopes
//! here are built through `api::method_request` — the same function the
//! provider calls. A change to the args the provider sends makes this
//! test miss its fixture and fail by name, which is the loud failure.

use std::sync::Arc;

use datalib_etl::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::ingest::api;
use datalib_etl_email::ingest::session::Session;
use datalib_etl_email::ingest::FetchOptions;
use serde_json::{json, Value};

use crate::support::{Mirror, Recorder};

const HOST: &str = "jmap.example.test";
const API_URL: &str = "https://jmap.example.test/jmap/api/";
const ACCOUNT: &str = "A1";
/// Enough to cross `Email/get`'s batch of 50, so the test also covers a
/// run whose ticks arrive in more than one chunk.
const MESSAGES: usize = 60;

/// Session, mailboxes, emails, threads, blobs — the five coarse ticks
/// the outer bar makes whatever a run turns out to hold. Mirrors
/// `ingest::PHASES`, which is private.
const PHASES: u64 = 5;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fastmail_enumeration_counts_down_from_its_message_count() {
    let m = Mirror::new();
    write_fixtures(&m.playback);

    let recorder = Recorder::default();
    let summary = m
        .run(|db| {
            let mut opts = FetchOptions::new(db);
            opts.hostname = HOST.to_string();
            opts.progress = Progress::new(Arc::new(recorder.clone()));
            datalib_etl_email::ingest::fetch(opts)
        })
        .await;

    let summary = summary.expect("jmap fetch under playback");
    assert_eq!(summary.emails_upserted, MESSAGES);
    assert_eq!(
        summary.blobs_downloaded, MESSAGES,
        "every message should have contributed one `.eml` download: {summary:?}",
    );

    let announced = recorder.announcements();
    // The old behaviour, and the thing this test exists to catch: the
    // only total ever announced was the phase count.
    assert!(
        announced.iter().any(|t| *t > PHASES),
        "the run never announced a total bigger than its {PHASES} phases, \
         so \"N queued\" counts down from four however big the mailbox is \
         (announced: {announced:?})",
    );
    // Messages enumerated, plus one `.eml` download each, plus the
    // phases. Exact rather than a lower bound: a total that overshoots
    // leaves the chip stuck above zero when the run ends.
    let expected = PHASES + MESSAGES as u64 * 2;
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        last, expected,
        "the last total announced was {last}, not {expected} \
         ({PHASES} phases + {MESSAGES} messages + {MESSAGES} blobs); \
         the chip would not reach zero (announced: {announced:?})",
    );
    assert_eq!(
        recorder.final_done(),
        expected,
        "the run ticked {} of the {expected} it announced, so \"N queued\" \
         ends the run above zero",
        recorder.final_done(),
    );
}

// ── fixtures ────────────────────────────────────────────────────────

fn message_id(i: usize) -> String {
    format!("M{i:04}")
}

fn blob_id(i: usize) -> String {
    format!("B{i:04}")
}

fn session() -> Session {
    Session::from_value(session_json()).expect("parse the fixture session")
}

fn session_json() -> Value {
    json!({
        "apiUrl": API_URL,
        "downloadUrl": "https://jmap.example.test/jmap/download/{accountId}/{blobId}/{name}?type={type}",
        "uploadUrl": "https://jmap.example.test/jmap/upload/{accountId}/",
        "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
        "accounts": { ACCOUNT: { "name": "t@example.test", "isPersonal": true } },
    })
}

fn write_fixtures(out: &std::path::Path) {
    let session = session();
    let put_call = |method: &str, args: Value, result: &Value| {
        let req = api::method_request(&session, method, args).expect("build the JMAP request");
        // A JMAP response wraps each method's result in `methodResponses`,
        // which is what `api::call` unwraps.
        let body = json!({ "methodResponses": [[method, result, "a"]] });
        write_fixture(out, &req, &json_response(&body)).expect("write fixture");
    };

    write_fixture(
        out,
        &HttpRequest::get(
            HttpService::Jmap,
            format!("https://{HOST}/.well-known/jmap"),
        ),
        &json_response(&session_json()),
    )
    .expect("write the session fixture");

    put_call(
        "Mailbox/get",
        json!({ "accountId": ACCOUNT, "ids": null }),
        &json!({
            "state": "mbox-1",
            "list": [{ "id": "MB1", "name": "Inbox", "role": "inbox", "parentId": null }],
        }),
    );

    let ids: Vec<String> = (0..MESSAGES).map(message_id).collect();
    put_call(
        "Email/query",
        json!({
            "accountId": ACCOUNT,
            "sort": [{ "property": "receivedAt", "isAscending": false }],
            "limit": 500,
            "position": 0,
            "calculateTotal": true,
        }),
        &json!({ "ids": ids, "queryState": "q-1", "total": MESSAGES }),
    );

    // `Email/get` runs in batches of 50, so a 60-message walk is two
    // calls and two fixtures.
    for batch in ids.chunks(50) {
        put_call(
            "Email/get",
            json!({
                "accountId": ACCOUNT,
                "ids": batch,
                "properties": [
                    "id", "blobId", "threadId", "mailboxIds", "keywords", "from",
                    "subject", "sentAt", "receivedAt", "size", "messageId",
                    "hasAttachment", "attachments",
                ],
            }),
            &json!({
                "state": "email-1",
                "list": batch.iter().map(|id| email(id)).collect::<Vec<_>>(),
            }),
        );
    }

    let thread_ids: Vec<String> = ids.iter().map(|id| format!("T{id}")).collect();
    put_call(
        "Thread/get",
        json!({ "accountId": ACCOUNT, "ids": thread_ids }),
        &json!({
            "state": "thread-1",
            "list": ids
                .iter()
                .map(|id| json!({ "id": format!("T{id}"), "emailIds": [id] }))
                .collect::<Vec<_>>(),
        }),
    );

    for i in 0..MESSAGES {
        let id = message_id(i);
        let url = session.download_url_for(ACCOUNT, &blob_id(i), "message.eml", "message/rfc822");
        write_fixture(
            out,
            &HttpRequest::get(HttpService::Jmap, url),
            &eml_response(&id),
        )
        .expect("write a blob fixture");
    }
}

fn email(id: &str) -> Value {
    let i: usize = id.trim_start_matches('M').parse().expect("numeric id");
    json!({
        "id": id,
        "blobId": blob_id(i),
        "threadId": format!("T{id}"),
        "mailboxIds": { "MB1": true },
        "keywords": { "$seen": true },
        "from": [{ "name": "Sender", "email": "sender@example.test" }],
        "subject": format!("message {id}"),
        "sentAt": "2026-09-01T10:00:00Z",
        "receivedAt": "2026-09-01T10:00:00Z",
        "size": 256,
        "messageId": [format!("{id}@example.test")],
        "hasAttachment": false,
        "attachments": [],
    })
}

fn eml_response(id: &str) -> HttpResponse {
    let eml = format!(
        "Message-ID: <{id}@example.test>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0000\r\n\
         From: sender@example.test\r\n\
         To: t@example.test\r\n\
         Subject: message {id}\r\n\
         \r\n\
         body of {id}\r\n",
    );
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("content-type".to_string(), "message/rfc822".to_string());
    HttpResponse {
        status: 200,
        headers,
        body: eml.into_bytes(),
        duration_ms: 0,
    }
}
