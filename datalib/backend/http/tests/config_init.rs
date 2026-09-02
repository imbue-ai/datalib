//! `POST /api/config/init` — the onboarding action that turns an empty
//! folder into a data library.
//!
//! The failure this guards against: a data root with no `config.toml`
//! declares no applets, so the grid's `/applet/unified_index/search`
//! came back `502 {"error":"no applet \"unified_index\""}` — the first
//! thing a new user saw. The config the endpoint writes is what makes
//! that applet exist at all, so the test that matters is not "a file
//! appeared" but "the gateway now knows the applet".
//!
//! What is asserted is the gateway's own bookkeeping —
//! configured-but-not-running is a different answer from
//! not-configured, and this test pins the transition between them. It
//! does not assert that the applet *starts*: the scaffold names
//! `datalib-applet` by bare command, which resolves to nothing under
//! `bazel test` and to a real binary on a machine that installed one,
//! and the property under test holds either way.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "config-init-test-token";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// The whole onboarding step: an empty root reports no config and
/// cannot serve the grid; one POST makes it a valid, applet-declaring
/// data library.
#[tokio::test]
async fn init_turns_an_empty_root_into_a_working_library() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state(tmp.path()).await);

    // Before: no config, and the grid's applet does not exist — the
    // reported symptom, reproduced.
    let (status, cfg) = call(&app, "GET", "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["exists"], false);
    let (status, err) = call(&app, "GET", "/applet/unified_index/search?q=&limit=1").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        err["error"].as_str().unwrap().contains("no applet"),
        "{err:?}"
    );

    let (status, init) = call(&app, "POST", "/api/config/init").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["created"], true, "{init:?}");
    assert_eq!(init["error"], serde_json::Value::Null);

    // After: the file is there, it validates through the runner's own
    // loader (`parsed_ok` is that check, not a syntax check), and it
    // declares no sources — an empty library, as advertised.
    let written = tmp.path().join("config.toml");
    assert!(written.is_file(), "expected {}", written.display());
    let (_, cfg) = call(&app, "GET", "/api/config").await;
    assert_eq!(cfg["exists"], true);
    assert_eq!(cfg["parsed_ok"], true, "{cfg:?}");
    assert_eq!(cfg["source_count"], 0);

    // The point of the whole exercise: the gateway now knows the
    // applet. Whether it can *run* it depends on the host — the
    // scaffold names `datalib-applet` by bare command, which resolves
    // to nothing in a sandbox and to a real binary on a dev machine
    // that installed one. Either outcome is a pass; what must be gone
    // is "no applet", the answer that sent people looking for a
    // missing feature instead of an unconfigured root.
    let (status, err) = call(&app, "GET", "/applet/unified_index/search?q=&limit=1").await;
    if status == StatusCode::BAD_GATEWAY {
        let msg = err["error"].as_str().unwrap();
        assert!(!msg.contains("no applet"), "still unconfigured: {msg}");
        assert!(msg.contains("unified_index"), "{msg}");
    }
}

/// Initializing twice must not overwrite the first library. The
/// endpoint is reachable from any window the user has open, and the
/// second one has no way to know the first already ran.
#[tokio::test]
async fn init_never_clobbers_an_existing_config() {
    let tmp = tempfile::tempdir().unwrap();
    let mine = "# hand-written\nsteps = []\n";
    std::fs::write(tmp.path().join("config.toml"), mine).unwrap();
    let app = router(state(tmp.path()).await);

    let (status, init) = call(&app, "POST", "/api/config/init").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["created"], false, "{init:?}");
    assert_eq!(init["error"], serde_json::Value::Null);
    assert_eq!(init["text"], mine);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml")).unwrap(),
        mine
    );
}

/// A root with a pre-TOML `config.yaml` is a migration waiting to
/// happen, not a fresh install. Writing an empty `config.toml` beside
/// it would silence the migration hint (`legacy_yaml_path` goes quiet
/// as soon as a TOML config exists) and leave the user with an empty
/// library plus a file full of sources nothing reads.
#[tokio::test]
async fn init_refuses_a_root_that_still_needs_migrating() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.yaml"), "sources: []\n").unwrap();
    let app = router(state(tmp.path()).await);

    let (status, init) = call(&app, "POST", "/api/config/init").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["created"], false, "{init:?}");
    let msg = init["error"].as_str().expect("a reason, not silence");
    assert!(msg.contains("config.yaml"), "{msg}");
    // The exact program string is resolved (absolute path inside a
    // bundle, bare name otherwise), so match the stem, not the whole
    // command.
    assert!(msg.contains("migrate"), "{msg}");
    assert!(
        !tmp.path().join("config.toml").exists(),
        "config.toml must not have been written"
    );
}
