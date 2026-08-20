//! Integration test for the frontend store and the applets that write
//! into it: `--write-frontend-dir`, namespace scanning, and the
//! endpoints the UI reads (`GET /api/frontend`, `GET /modules/{hash}`).
//!
//! The fixture applet is a `sh` script the test writes, which is the
//! point: an applet's whole obligation is to leave two kinds of file in
//! a directory, and a shell script proves nothing Rust-specific leaked
//! into that contract.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::dolt_repo::DoltRepo;
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_http::applets::AppletRegistry;
use datalib_http::frontend::{frontend_dir, FrontendStore};
use datalib_http::sha256_hex;
use datalib_http::ApiToken;
use datalib_http::{router, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "frontend-test-token";

/// The component every fixture writes. Its bytes are the same for all
/// of them, so two namespaces share one address.
const COMPONENT: &str = "export default (id) => (root, ctx) => () => {};\n";

fn component_hash() -> String {
    sha256_hex(COMPONENT.as_bytes())
}

/// A fixture applet: writes `<hash>.js` plus `<name>.json` into
/// whatever directory it is handed, taking the namespace from that
/// directory's last segment — exactly what the reference applet does.
fn write_fixture(dir: &Path, script_name: &str) -> PathBuf {
    let hash = component_hash();
    let script = format!(
        r#"#!/bin/sh
set -e
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --write-frontend-dir) out="$2"; shift ;;
  esac
  shift
done
[ -n "$out" ] || exit 64
ns=$(basename "$out")
mkdir -p "$out"
printf '%s' 'export default (id) => (root, ctx) => () => {{}};
' > "$out/{hash}.js"
cat > "$out/view.json" <<META
{{"title":"Fixture $ns","description":"d","component_hash":"{hash}","component_args":["$ns"]}}
META
"#
    );
    let path = dir.join(script_name);
    std::fs::write(&path, script).unwrap();
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
    let dolt = DoltRepo::open(&db_path, root.clone()).await.unwrap();
    AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
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

/// Write a component into the `user` namespace by hand — the same two
/// files an applet writes, which is the property the whole design rests
/// on.
fn seed_user(root: &Path, name: &str, title: &str) -> String {
    let dir = frontend_dir(root).join("user");
    std::fs::create_dir_all(&dir).unwrap();
    let hash = component_hash();
    std::fs::write(dir.join(format!("{hash}.js")), COMPONENT).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        format!(
            r#"{{"title":"{title}","description":"by hand","component_hash":"{hash}","component_args":[]}}"#
        ),
    )
    .unwrap();
    hash
}

#[tokio::test]
async fn an_applet_writes_a_namespace_and_it_is_served() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fixture(tmp.path(), "fixture.sh");
    let cfg = format!(
        "[[applets]]\nid = \"demo\"\ncommand = \"sh {}\"\n",
        script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    let (status, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(status, StatusCode::OK);
    let entry = &view["namespaces"]["demo"]["entries"]["view"];
    assert_eq!(entry["title"], "Fixture demo");
    assert_eq!(entry["component_hash"], component_hash());
    // The argument the gallery will pass is the namespace itself — the
    // only thing distinguishing two instances of one command.
    assert_eq!(entry["component_args"], serde_json::json!(["demo"]));
    assert!(view["namespaces"]["demo"]["problems"].is_null());

    // …and the component is served by content hash.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/modules/{}", component_hash()))
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
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), COMPONENT.as_bytes());
}

/// A component written by hand and one written by an applet are read
/// the same way. This is the whole claim of the design.
#[tokio::test]
async fn a_hand_written_namespace_reads_like_an_applet_one() {
    let tmp = tempfile::tempdir().unwrap();
    seed_user(tmp.path(), "tetris", "Tetris");
    let script = write_fixture(tmp.path(), "fixture.sh");
    let cfg = format!(
        "[[applets]]\nid = \"demo\"\ncommand = \"sh {}\"\n",
        script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    let user = &view["namespaces"]["user"]["entries"]["tetris"];
    let demo = &view["namespaces"]["demo"]["entries"]["view"];
    // Same shape, same fields, same content hash — one mechanism.
    assert_eq!(user["component_hash"], demo["component_hash"]);
    assert_eq!(user["title"], "Tetris");
    assert_eq!(demo["title"], "Fixture demo");
}

/// Two instances of one command: one component address, two namespaces,
/// each carrying its own argument.
#[tokio::test]
async fn two_instances_share_a_component_and_own_their_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fixture(tmp.path(), "fixture.sh");
    let cfg = format!(
        "[[applets]]\nid = \"a\"\ncommand = \"sh {p}\"\n\n[[applets]]\nid = \"b\"\ncommand = \"sh {p}\"\n",
        p = script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, view) = get_json(&app, "/api/frontend").await;

    let a = &view["namespaces"]["a"]["entries"]["view"];
    let b = &view["namespaces"]["b"]["entries"]["view"];
    assert_eq!(a["component_hash"], b["component_hash"]);
    assert_eq!(a["component_args"], serde_json::json!(["a"]));
    assert_eq!(b["component_args"], serde_json::json!(["b"]));
}

/// A refresh deletes applet namespaces so the store tracks the config —
/// and must leave `user` alone, which is why that id is reserved.
#[tokio::test]
async fn a_refresh_rebuilds_applet_namespaces_and_spares_user() {
    let tmp = tempfile::tempdir().unwrap();
    seed_user(tmp.path(), "tetris", "Tetris");
    let script = write_fixture(tmp.path(), "fixture.sh");
    let cfg_path = tmp.path().join("config.toml");
    let with_gone = format!(
        "[[applets]]\nid = \"stays\"\ncommand = \"sh {p}\"\n\n[[applets]]\nid = \"goes\"\ncommand = \"sh {p}\"\n",
        p = script.display()
    );
    let app = router(state_with(tmp.path(), &with_gone).await);

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(view["namespaces"]["goes"].is_object());

    // Drop one applet from the config.
    std::fs::write(
        &cfg_path,
        format!(
            "[[applets]]\nid = \"stays\"\ncommand = \"sh {}\"\n",
            script.display()
        ),
    )
    .unwrap();
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

/// A filename is a claim about the bytes; an unchecked claim would
/// serve stale code forever from a URL that promises immutability.
#[tokio::test]
async fn a_component_whose_name_lies_is_skipped_and_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = frontend_dir(tmp.path()).join("user");
    std::fs::create_dir_all(&dir).unwrap();
    let lie = "0".repeat(64);
    std::fs::write(dir.join(format!("{lie}.js")), "export default 1;").unwrap();
    std::fs::write(
        dir.join("x.json"),
        format!(r#"{{"title":"X","component_hash":"{lie}"}}"#),
    )
    .unwrap();

    let app = router(state_with(tmp.path(), "").await);
    let (_, view) = get_json(&app, "/api/frontend").await;
    let user = &view["namespaces"]["user"];
    assert!(user["entries"].as_object().unwrap().is_empty());
    // Both the bad file and the metadata left dangling by it are said
    // out loud — a component that silently fails to appear looks
    // identical to one that was never written.
    assert_eq!(user["problems"].as_array().unwrap().len(), 2);
}

/// A broken applet must not take the others down, and must be named.
#[tokio::test]
async fn a_failing_applet_is_reported_without_hiding_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let good = write_fixture(tmp.path(), "good.sh");
    let bad = tmp.path().join("bad.sh");
    std::fs::write(&bad, "#!/bin/sh\necho 'boom' >&2\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = format!(
        "[[applets]]\nid = \"ok\"\ncommand = \"sh {}\"\n\n[[applets]]\nid = \"broken\"\ncommand = \"sh {}\"\n",
        good.display(),
        bad.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, view) = get_json(&app, "/api/frontend").await;

    assert!(view["namespaces"]["ok"]["entries"]["view"].is_object());
    let err = view["applet_errors"]["broken"].as_str().unwrap();
    // The child's stderr is what tells the user what to fix.
    assert!(err.contains("boom"), "stderr not surfaced: {err}");
}

/// A config edit shows up without a restart.
#[tokio::test]
async fn a_config_edit_is_picked_up_without_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fixture(tmp.path(), "fixture.sh");
    let cfg_path = tmp.path().join("config.toml");
    let app = router(
        state_with(
            tmp.path(),
            &format!(
                "[[applets]]\nid = \"first\"\ncommand = \"sh {}\"\n",
                script.display()
            ),
        )
        .await,
    );

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert!(view["namespaces"]["second"].is_null());

    std::fs::write(
        &cfg_path,
        format!(
            "[[applets]]\nid = \"first\"\ncommand = \"sh {p}\"\n\n[[applets]]\nid = \"second\"\ncommand = \"sh {p}\"\n",
            p = script.display()
        ),
    )
    .unwrap();

    let (_, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(
        view["namespaces"]["second"]["entries"]["view"]["component_args"],
        serde_json::json!(["second"]),
        "the edit was not picked up"
    );
}

/// Refreshing sits on a polled endpoint, so an unchanged config must
/// not re-exec every applet each tick.
#[tokio::test]
async fn an_unchanged_config_is_not_rebuilt() {
    let tmp = tempfile::tempdir().unwrap();
    let inner = write_fixture(tmp.path(), "inner.sh");
    let counter = tmp.path().join("runs.log");
    let wrapper = tmp.path().join("wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho run >> {}\nexec sh {} \"$@\"\n",
            counter.display(),
            inner.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let app = router(
        state_with(
            tmp.path(),
            &format!(
                "[[applets]]\nid = \"a\"\ncommand = \"sh {}\"\n",
                wrapper.display()
            ),
        )
        .await,
    );
    let runs = || {
        std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(runs(), 1, "boot should write exactly once");

    for _ in 0..5 {
        let _ = get_json(&app, "/api/frontend").await;
    }
    assert_eq!(
        runs(),
        1,
        "polling re-ran the applets on an unchanged config"
    );
}

#[tokio::test]
async fn no_applets_and_no_store_is_an_empty_view() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    let (status, view) = get_json(&app, "/api/frontend").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["namespaces"], serde_json::json!({}));
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
    seed_user(tmp.path(), "tetris", "Tetris");
    let app = router(state_with(tmp.path(), "").await);

    for uri in [
        "/api/frontend".to_string(),
        format!("/modules/{}", component_hash()),
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

/// A `--write-frontend-dir` that never exits must not hang the boot:
/// the refresh runs after the listener is already bound, so the symptom
/// would be a tab whose requests queue with nothing logged.
#[tokio::test]
async fn a_write_that_never_exits_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("hang.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 600\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = datalib_dag::config::parse(&format!(
        "[[applets]]\nid = \"hang\"\ncommand = \"sh {}\"\n",
        script.display()
    ))
    .unwrap();

    let started = std::time::Instant::now();
    let err = datalib_http::applets::write_frontend_dir_with_timeout(
        &cfg.applets[0],
        tmp.path(),
        None,
        &frontend_dir(tmp.path()).join("hang"),
        std::time::Duration::from_millis(400),
    )
    .expect_err("a hanging write must not succeed");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(err.to_string().contains("did not exit"), "unhelpful: {err}");
}

/// The store is just files: a directory dropped in by hand, with no
/// applet and no config at all, is a namespace.
#[tokio::test]
async fn a_directory_alone_defines_a_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = frontend_dir(tmp.path()).join("scratch");
    std::fs::create_dir_all(&dir).unwrap();
    let hash = component_hash();
    std::fs::write(dir.join(format!("{hash}.js")), COMPONENT).unwrap();
    std::fs::write(
        dir.join("thing.json"),
        format!(r#"{{"title":"Thing","component_hash":"{hash}","component_args":[1,true]}}"#),
    )
    .unwrap();

    let store = FrontendStore::scan(tmp.path());
    let view = store.view();
    assert!(view.contains_key("scratch"));
    assert!(store.read_component(&hash).is_some());
}
