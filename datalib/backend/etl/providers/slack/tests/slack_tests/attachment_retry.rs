//! Two-run tests for the attachment retry pass: a file that did not land
//! on one run is tried again on the next from the stored message, which
//! the second run never lists again.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl::blob_cas::blake3_hex;
use datalib_etl::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl::synthesize::write_fixture;
use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{record_call, History};
use serde_json::{json, Value};

use crate::support::{fetch_into, record_general, Tree};

const TS: &str = "1735689600.000100";
const FILE_ID: &str = "F_DELTA_SHIELD";
const URL: &str = "https://files.slack.com/files-pri/T1-F_DELTA_SHIELD/download/delta_shield.png";
const BYTES: &[u8] = b"\x89PNG not really a delta shield";

fn message_with_file() -> Value {
    json!({
        "ts": TS,
        "user": "U1",
        "text": "The insignia for the away team briefing",
        "files": [{
            "id": FILE_ID,
            "name": "delta_shield.png",
            "mimetype": "image/png",
            "size": BYTES.len(),
            "url_private_download": URL,
        }],
    })
}

fn serve_file(playback: &Path, status: u16, body: &[u8]) {
    write_fixture(
        playback,
        &HttpRequest::get(HttpService::Slack, URL),
        &HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: body.to_vec(),
            duration_ms: 0,
        },
    )
    .unwrap();
}

/// Run 1's world: `#general` holding the one message with a file.
fn first_world(file_status: u16) -> Tree {
    let t = Tree::new();
    record_general(&t.api);
    History::cold("C1")
        .record(&t.api, json!([message_with_file()]))
        .unwrap();
    t.serve();
    serve_file(&t.playback, file_status, BYTES);
    t
}

/// Run 1's world where the walk fails after its first page: the page
/// names a next one, and nothing serves it.
fn first_world_failing_after_the_first_page(file_status: u16) -> Tree {
    let t = Tree::new();
    record_general(&t.api);
    record_call(
        &t.api,
        "conversations.history",
        json!({
            "channel": "C1",
            "include_all_metadata": "true",
            "inclusive": "true",
            "limit": "200",
            "oldest": datalib_etl_slack::recorded::DEFAULT_SINCE_TS,
        }),
        json!({
            "ok": true,
            "messages": [message_with_file()],
            "has_more": true,
            "response_metadata": {"next_cursor": "page2"},
        }),
    )
    .unwrap();
    t.serve();
    serve_file(&t.playback, file_status, BYTES);
    t
}

/// Run 2's world: the forward walk resumes after the stored message and
/// finds nothing. Nothing else is served, so a run that lists the
/// message again fails its channel.
fn second_world() -> Tree {
    let t = Tree::new();
    record_general(&t.api);
    History {
        inclusive: false,
        ..History::from("C1", TS)
    }
    .record(&t.api, json!([]))
    .unwrap();
    t.serve();
    serve_file(&t.playback, 200, BYTES);
    t
}

#[derive(Debug, PartialEq)]
struct Attachment {
    blake3: Option<String>,
    /// `(severity, reason)` of its fetch problem, if it has one.
    problem: Option<(String, String)>,
    bytes: Option<Vec<u8>>,
}

async fn attachment(out: &Path) -> Attachment {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let (blake3, severity, reason): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT a.blake3, p.severity, p.reason FROM slack_attachments a \
             LEFT JOIN problems p ON p.scope_key = 'slack_attachments:' || a.id \
             WHERE a.file_id = ?",
        )
        .bind(FILE_ID)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let bytes = match &blake3 {
        Some(h) => db.cas().get(h).await.unwrap().map(|o| o.bytes),
        None => None,
    };
    db.close().await;
    Attachment {
        blake3,
        problem: severity.zip(reason),
        bytes,
    }
}

fn landed() -> Attachment {
    Attachment {
        blake3: Some(blake3_hex(BYTES)),
        problem: None,
        bytes: Some(BYTES.to_vec()),
    }
}

async fn run(out: &Path, limit: Option<u64>) -> usize {
    let summary = fetch_into(out, |o| FetchOptions {
        media: true,
        blob_size_limit_bytes: limit,
        ..o
    })
    .await
    .unwrap();
    summary.messages
}

/// A file whose download failed is fetched on the next run, although
/// the resume cursor has passed its message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_download_is_retried_without_relisting_its_message() {
    let t = first_world(500);
    run(&t.out, None).await;
    let first = attachment(&t.out).await;
    assert_eq!(first.blake3, None);
    assert_eq!(
        first.problem,
        Some(("error".to_string(), "fetch_failed".to_string())),
        "a file that never landed is dropped, not stale"
    );

    let _second = second_world();
    assert_eq!(
        run(&t.out, None).await,
        0,
        "the message is not listed again"
    );
    assert_eq!(attachment(&t.out).await, landed());
}

/// A file skipped for being over the size limit is fetched once the
/// limit is raised, without a re-walk of the channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raised_size_limit_fetches_a_skipped_file_without_a_rewalk() {
    let t = first_world(200);
    run(&t.out, Some(4)).await;
    let first = attachment(&t.out).await;
    assert_eq!(first.blake3, None);
    assert_eq!(
        first.problem,
        Some(("warning".to_string(), "over_size_limit".to_string()))
    );

    let _second = second_world();
    assert_eq!(
        run(&t.out, None).await,
        0,
        "the message is not listed again"
    );
    assert_eq!(attachment(&t.out).await, landed());
}

/// A file recorded as a failed fetch that today's limit would skip is
/// re-judged, and its problem says so: the shape a store written before
/// size skips had their own reason is left in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_file_over_todays_limit_is_reclassified_as_a_skip() {
    let t = first_world(500);
    run(&t.out, None).await;

    let _second = second_world();
    run(&t.out, Some(4)).await;
    let after = attachment(&t.out).await;
    assert_eq!(after.blake3, None);
    assert_eq!(
        after.problem,
        Some(("warning".to_string(), "over_size_limit".to_string()))
    );
}

/// A channel whose walk fails partway still writes the attachments of
/// the messages it stored, so the retry pass can find them. It used to
/// return before its flush: the messages were stored, their files had
/// no row, and the resume cursor had passed them for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_that_fails_partway_still_records_its_attachments() {
    let t = first_world_failing_after_the_first_page(500);
    run(&t.out, None).await;
    let first = attachment(&t.out).await;
    assert_eq!(
        first.problem,
        Some(("error".to_string(), "fetch_failed".to_string()))
    );

    let _second = second_world();
    run(&t.out, None).await;
    assert_eq!(attachment(&t.out).await, landed());
}
