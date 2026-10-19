//! An access token that expires is renewed once by signing in again, and
//! the new token is used only when it is for the account the source holds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use datalib_etl_chatgpt::ingest::ChatGPTClient;
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::write_fixture;
use serde_json::{json, Value};

const ME: &str = "/backend-api/me";
const LISTING: &str = "/backend-api/conversations?offset=0&limit=100&order=updated";

fn answer(tape: &Path, path: &str, status: u16, body: &Value) {
    let req = HttpRequest::get(HttpService::Chatgpt, format!("https://chatgpt.com{path}"))
        .header("Accept", "application/json");
    let resp = HttpResponse {
        status,
        headers: Default::default(),
        body: serde_json::to_vec(body).unwrap(),
        duration_ms: 0,
    };
    write_fixture(tape, &req, &resp).unwrap();
}

fn expired(tape: &Path, path: &str) {
    answer(
        tape,
        path,
        401,
        &json!({"detail": {"code": "token_expired"}}),
    );
}

/// A sign-in that makes chatgpt.com answer as `account`, counting itself.
fn sign_in_as(
    tape: &Path,
    account: &'static str,
    count: &Arc<AtomicUsize>,
) -> impl Fn() -> std::future::Ready<anyhow::Result<()>> + Send + Sync + 'static {
    let (tape, count): (PathBuf, _) = (tape.to_path_buf(), count.clone());
    move || {
        count.fetch_add(1, Ordering::SeqCst);
        answer(&tape, ME, 200, &json!({"id": account}));
        answer(&tape, LISTING, 200, &json!({"items": []}));
        std::future::ready(Ok(()))
    }
}

/// The access token lasts about ten days, and every sync after that
/// failed `/me` with a 401 until a person signed in again by hand.
#[tokio::test]
async fn an_expired_token_is_renewed_once_and_each_request_retried() {
    let tape = tempfile::tempdir().unwrap();
    expired(tape.path(), ME);
    expired(tape.path(), LISTING);
    let count = Arc::new(AtomicUsize::new(0));
    let client = ChatGPTClient::new()
        .holding_account(Some("u-1".into()))
        .renew_with(sign_in_as(tape.path(), "u-1", &count));

    let (me, page) = playback::scope(tape.path(), async {
        tokio::join!(client.me(), client.list_conversations_page(0, 100))
    })
    .await;
    assert_eq!(me.unwrap()["id"], "u-1");
    assert_eq!(page.unwrap()["items"], json!([]));
    assert_eq!(count.load(Ordering::SeqCst), 1, "one sign-in serves both");
}

/// latchkey keeps one browser session for every account, so a sign-in
/// can come back as someone else.
#[tokio::test]
async fn a_sign_in_as_another_account_is_not_used() {
    let tape = tempfile::tempdir().unwrap();
    expired(tape.path(), LISTING);
    let count = Arc::new(AtomicUsize::new(0));
    let client = ChatGPTClient::new()
        .holding_account(Some("u-1".into()))
        .renew_with(sign_in_as(tape.path(), "u-2", &count));

    let err = playback::scope(tape.path(), client.list_conversations_page(0, 100))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("HTTP 401"),
        "the listing is not retried: {err}"
    );
    assert!(err.contains("u-2"), "{err}");
}

/// With no account to check a sign-in against, the 401 stands.
#[tokio::test]
async fn a_client_that_holds_no_account_does_not_sign_in() {
    let tape = tempfile::tempdir().unwrap();
    expired(tape.path(), ME);
    let count = Arc::new(AtomicUsize::new(0));
    let client = ChatGPTClient::new().renew_with(sign_in_as(tape.path(), "u-1", &count));

    let err = playback::scope(tape.path(), client.me()).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 401"), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// Each sign-in can put a browser window on the screen; one that failed
/// is not tried again for every later request of the run.
#[tokio::test]
async fn a_failed_sign_in_is_tried_once() {
    let tape = tempfile::tempdir().unwrap();
    expired(tape.path(), ME);
    expired(tape.path(), LISTING);
    let count = Arc::new(AtomicUsize::new(0));
    let tries = count.clone();
    let client = ChatGPTClient::new()
        .holding_account(Some("u-1".into()))
        .renew_with(move || {
            tries.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(anyhow::anyhow!("the browser session has expired too")))
        });

    for path in [ME, LISTING] {
        let err = playback::scope(tape.path(), client.get(path))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP 401"), "{err}");
        assert!(err.contains("session has expired too"), "{err}");
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
