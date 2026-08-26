//! Integration test for the frontend store and the applets that fill
//! it: eager start, namespace scanning, and the endpoints the UI reads
//! (`GET /api/frontend`, `GET /modules/{hash}`).
//!
//! An applet is one invocation now — write the directory, then bind the
//! port — so a fixture that only writes is no longer a valid applet.
//! Working fixtures are the real `datalib-applet` binary; `sh` scripts
//! cover the failure paths, where never binding is the whole point.
//!
//! Pure store semantics (a `.js` whose name lies about its bytes,
//! metadata naming a component that is not there) are unit-tested in
//! `datalib/backend/http/src/frontend.rs`, where they need no processes
//! and stay hermetic.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::dolt_repo::{AppStore, DoltRepo};
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_http::applets::AppletRegistry;
use datalib_http::frontend::frontend_dir;
use datalib_http::sha256_hex;
use datalib_http::ApiToken;
use datalib_http::{router, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "frontend-test-token";

/// The real applet host, built by Bazel and handed over in `env`,
/// spelled the way a config would: binary plus subcommand.
fn applet_command() -> String {
    let bin = PathBuf::from(std::env::var("APPLET_BIN").expect("APPLET_BIN set by the BUILD rule"))
        .canonicalize()
        .expect("applet binary exists");
    format!("{} slack", bin.display())
}

/// A rendered tree the slack applet can read, so a started applet has
/// something to report.
fn seed_tree(root: &Path) {
    let tree = root.join("slack/rendered_md");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(
        tree.join("md1.grid_rows.json"),
        r##"{"rows":[
          {"channel":"#eng","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"md1","message_index":null,"text":"a thread"},
          {"channel":"#eng","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"md1","message_index":0,"author":"ann","text":"a thread"}
        ]}"##,
    )
    .unwrap();
}

/// A config declaring one instance of the real applet per id.
fn config_for(ids: &[&str]) -> String {
    let bin = applet_command();
    ids.iter()
        .map(|id| {
            format!(
                "[[applets]]\nid = \"{id}\"\ncommand = \"{bin}\"\n[applets.params]\ntree = \"slack/rendered_md\"\n\n"
            )
        })
        .collect()
}

/// A component written by hand into `user` — the same two files an
/// applet writes, which is the property the whole design rests on.
fn seed_user(root: &Path, name: &str, title: &str) -> String {
    let dir = frontend_dir(root).join("user");
    std::fs::create_dir_all(&dir).unwrap();
    let body = "export default () => (root, ctx) => () => {};\n";
    let hash = sha256_hex(body.as_bytes());
    std::fs::write(dir.join(format!("{hash}.js")), body).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        format!(
            r#"{{"title":"{title}","description":"by hand","component_hash":"{hash}","component_args":[]}}"#
        ),
    )
    .unwrap();
    hash
}

/// An executable that is not a valid applet, for the failure paths.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

async fn state_with(root: &Path, config_toml: &str) -> AppState {
    std::fs::write(root.join("config.toml"), config_toml).unwrap();
    let db_path = root.join("backend_index.doltlite_db");
    let root = Arc::new(root.to_path_buf());
    let dolt = DoltRepo::open(root.clone()).await.unwrap();
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        app: Arc::new(app),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
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

/// The whole loop: the applet is started, writes its namespace before
/// binding, and the store scan that follows finds it.
#[tokio::test]
async fn a_started_applet_has_already_written_its_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    let app = router(state_with(tmp.path(), &config_for(&["demo"])).await);

    let (status, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(status, StatusCode::OK);
    assert!(view["applet_errors"].is_null(), "{view}");
    let entry = &view["namespaces"]["demo"]["entries"]["channels"];
    // The argument the gallery will pass is the namespace itself — the
    // only thing distinguishing two instances of one command.
    assert_eq!(entry["component_args"], serde_json::json!(["demo"]));
    let hash = entry["component_hash"].as_str().unwrap().to_string();

    // …and the component is served by content hash.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/modules/{hash}"))
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );

    // The same process is already serving data — one invocation.
    let (status, body) = get_json(&app, "/applet/demo/channels").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["channels"][0]["name"], "#eng");
}

/// A component written by hand and one written by an applet are read
/// the same way. This is the whole claim of the design.
#[tokio::test]
async fn a_hand_written_namespace_reads_like_an_applet_one() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    seed_user(tmp.path(), "tetris", "Tetris");
    let app = router(state_with(tmp.path(), &config_for(&["demo"])).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    let user = &view["namespaces"]["user"]["entries"]["tetris"];
    let demo = &view["namespaces"]["demo"]["entries"]["channels"];
    // Same shape, same fields — one mechanism.
    assert_eq!(user["title"], "Tetris");
    assert!(user["component_hash"].is_string());
    assert!(demo["component_hash"].is_string());
}

/// Two instances of one command: one component address, two
/// namespaces, each carrying its own argument.
#[tokio::test]
async fn two_instances_share_a_component_and_own_their_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    let app = router(state_with(tmp.path(), &config_for(&["a", "b"])).await);
    let (_, view) = get_json(&app, "/api/frontend").await;

    let a = &view["namespaces"]["a"]["entries"]["channels"];
    let b = &view["namespaces"]["b"]["entries"]["channels"];
    assert_eq!(a["component_hash"], b["component_hash"]);
    assert_eq!(a["component_args"], serde_json::json!(["a"]));
    assert_eq!(b["component_args"], serde_json::json!(["b"]));

    // Both are serving, independently.
    for id in ["a", "b"] {
        let (status, _) = get_json(&app, &format!("/applet/{id}/channels")).await;
        assert_eq!(status, StatusCode::OK, "{id}");
    }
}

/// A restart rebuilds applet namespaces so the store tracks the config
/// — and must leave `user` alone, which is why that id is reserved.
#[tokio::test]
async fn a_restart_rebuilds_applet_namespaces_and_spares_user() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    seed_user(tmp.path(), "tetris", "Tetris");
    let app = router(state_with(tmp.path(), &config_for(&["stays", "goes"])).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(view["namespaces"]["goes"].is_object());

    // Drop one applet from the config.
    std::fs::write(tmp.path().join("config.toml"), config_for(&["stays"])).unwrap();
    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(
        view["namespaces"]["goes"].is_null(),
        "a removed applet must take its components with it"
    );
    assert!(view["namespaces"]["stays"].is_object());
    // The hand-authored namespace is untouched by any of this.
    assert_eq!(
        view["namespaces"]["user"]["entries"]["tetris"]["title"],
        "Tetris"
    );
    assert!(frontend_dir(tmp.path()).join("user").is_dir());
}

/// A broken applet must not take the others down, and must be named.
#[tokio::test]
async fn a_failing_applet_is_reported_without_hiding_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    let bad = write_script(tmp.path(), "bad.sh", "#!/bin/sh\necho 'boom' >&2\nexit 3\n");
    let cfg = format!(
        "{}[[applets]]\nid = \"broken\"\ncommand = \"sh {}\"\n",
        config_for(&["ok"]),
        bad.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, view) = get_json(&app, "/api/frontend").await;

    assert!(view["namespaces"]["ok"]["entries"]["channels"].is_object());
    let err = view["applet_errors"]["broken"].as_str().unwrap();
    // The child's stderr is what tells the user what to fix.
    assert!(err.contains("boom"), "stderr not surfaced: {err}");

    // Requesting it says it is configured but not running, which is a
    // different failure from not being configured at all.
    let (status, body) = get_json(&app, "/applet/broken/x").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("broken"), "{body}");

    let (status, body) = get_json(&app, "/applet/nope/x").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"].as_str().unwrap().contains("no applet"),
        "{body}"
    );
}

/// An applet that never binds must not hang the boot: the start runs
/// after the listener is already accepting, so an unbounded wait would
/// leave a tab whose requests queue with nothing logged.
#[tokio::test]
async fn an_applet_that_never_binds_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(tmp.path(), "hang.sh", "#!/bin/sh\nsleep 600\n");
    let cfg = format!(
        "[[applets]]\nid = \"hang\"\ncommand = \"sh {}\"\n",
        script.display()
    );
    let started = std::time::Instant::now();
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, view) = get_json(&app, "/api/frontend").await;

    // The production bound is 20s; the point is that it is bounded at
    // all, and that boot survives it.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "boot waited {:?}",
        started.elapsed()
    );
    let err = view["applet_errors"]["hang"].as_str().unwrap();
    assert!(err.contains("did not start listening"), "{err}");
}

/// A config edit shows up without restarting the server.
#[tokio::test]
async fn a_config_edit_is_picked_up_without_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    let app = router(state_with(tmp.path(), &config_for(&["first"])).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(view["namespaces"]["second"].is_null());

    std::fs::write(
        tmp.path().join("config.toml"),
        config_for(&["first", "second"]),
    )
    .unwrap();

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(
        view["namespaces"]["second"]["entries"]["channels"]["component_args"],
        serde_json::json!(["second"]),
        "the edit was not picked up"
    );
    // …and the newly-started applet is serving.
    let (status, _) = get_json(&app, "/applet/second/channels").await;
    assert_eq!(status, StatusCode::OK);
}

/// Restarting sits on a polled endpoint, so an unchanged config must
/// not restart every applet each tick.
#[tokio::test]
async fn an_unchanged_config_does_not_restart_anything() {
    let tmp = tempfile::tempdir().unwrap();
    seed_tree(tmp.path());
    let counter = tmp.path().join("starts.log");
    let wrapper = write_script(
        tmp.path(),
        "wrapper.sh",
        &format!(
            "#!/bin/sh\necho start >> {}\nexec {} \"$@\"\n",
            counter.display(),
            applet_command()
        ),
    );
    let cfg = format!(
        "[[applets]]\nid = \"a\"\ncommand = \"sh {}\"\n[applets.params]\ntree = \"slack/rendered_md\"\n",
        wrapper.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let starts = || {
        std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(starts(), 1, "boot should start exactly once");

    for _ in 0..5 {
        let _ = get_json(&app, "/api/frontend").await;
    }
    assert_eq!(starts(), 1, "polling restarted the applets");
}

#[tokio::test]
async fn no_applets_and_no_store_is_an_empty_view() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    let (status, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["namespaces"], serde_json::json!({}));
}

/// A hand-written component needs no applet, no config, and no
/// restart — the store is just files.
#[tokio::test]
async fn the_user_namespace_needs_no_applet_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    let hash = seed_user(tmp.path(), "hand", "Hand-written");

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(
        view["namespaces"]["user"]["entries"]["hand"]["title"],
        "Hand-written"
    );
    let resp = app
        .oneshot(
            Request::get(format!("/modules/{hash}"))
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn component_paths_cannot_escape_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    for bad in [
        "not-a-hash".to_string(),
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(64),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/modules/{bad}"))
                    .header("x-datalib-token", TEST_TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "/modules/{bad}");
    }
}

/// These routes serve executable JavaScript and proxy to a
/// config-named program — exactly the surface the token gate exists to
/// protect. The gate is an outermost layer, so this holds by
/// construction; the test is here so a route added outside it fails
/// loudly.
#[tokio::test]
async fn frontend_routes_are_behind_the_token_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let hash = seed_user(tmp.path(), "tetris", "Tetris");
    let app = router(state_with(tmp.path(), "").await);

    for uri in [
        "/api/frontend".to_string(),
        format!("/modules/{hash}"),
        "/applet/demo/anything".to_string(),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} answered without a token"
        );
    }
}
