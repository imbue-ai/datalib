//! Integration test for the applet gateway: discovery via
//! `--frontend-manifest`, the flat content-addressed module store, and
//! the endpoints the UI reads (`GET /api/applets`, `GET
//! /modules/{hash}`).
//!
//! The fixture applet is a `sh` script the test writes, which is the
//! point: the contract is "any executable that takes the flags and
//! prints the JSON", and a shell script is the cheapest possible proof
//! that nothing Rust-specific leaked into it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::dolt_repo::DoltRepo;
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_http::applets::AppletRegistry;
use datalib_http::sha256_hex;
use datalib_http::{router, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

/// A fixture applet: writes `module_body` into `--module-dir` under
/// `file_name`, then prints a manifest claiming `claimed_hash`. The
/// two are separate parameters so a test can write bytes under a name
/// that does not describe them and watch verification catch it.
fn write_fixture(
    dir: &Path,
    name: &str,
    module_body: &str,
    file_name: &str,
    claimed_hash: &str,
) -> PathBuf {
    let real = file_name;
    let script = format!(
        r#"#!/bin/sh
set -e
moduledir=""
appletid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --module-dir) moduledir="$2"; shift ;;
    --applet-id) appletid="$2"; shift ;;
  esac
  shift
done
mkdir -p "$moduledir"
cat > "$moduledir/{real}" <<'MODULE_EOF'
{module_body}
MODULE_EOF
cat <<MANIFEST_EOF
{{"components":[{{"name":"view","module":"{claimed_hash}"}}],
  "gallery":[{{"source":"$appletid.view(\"$appletid\")","title":"Fixture $appletid","description":"d"}}]}}
MANIFEST_EOF
"#
    );
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The heredoc above appends a trailing newline, so the bytes on disk
/// are the body plus "\n" — hash what will actually be written.
fn stored_bytes(body: &str) -> String {
    format!("{body}\n")
}

async fn state_with(root: &Path, config_toml: &str) -> AppState {
    let db_path = root.join("backend_index.doltlite_db");
    let root = Arc::new(root.to_path_buf());
    let dolt = DoltRepo::open(&db_path, root.clone()).await.unwrap();
    let cfg = datalib_dag::config::parse(config_toml).expect("fixture config parses");
    datalib_dag::config::validate_applets(&cfg).expect("fixture config is valid");
    AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        applets: Arc::new(AppletRegistry::discover(cfg.applets, (*root).clone(), None)),
    }
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
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

#[tokio::test]
async fn discovers_an_applet_and_serves_its_module() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "export default (id) => (root, ctx) => () => {};";
    let hash = sha256_hex(stored_bytes(body).as_bytes());
    let script = write_fixture(tmp.path(), "fixture.sh", body, &hash, &hash);

    let cfg = format!(
        "[[applets]]\nid = \"demo\"\ntitle = \"Demo\"\ncommand = \"sh {}\"\n",
        script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);

    let (status, applets) = get_json(&app, "/api/applets").await;
    assert_eq!(status, StatusCode::OK);
    let a = &applets.as_array().unwrap()[0];
    assert_eq!(a["id"], "demo");
    assert_eq!(a["title"], "Demo");
    assert!(a.get("error").is_none(), "unexpected error: {a}");
    assert_eq!(a["components"]["view"], hash);
    // The gallery entry is a full snippet, and it names the instance
    // it came from — that is what the applet needed --applet-id for.
    assert_eq!(a["gallery"][0]["source"], r#"demo.view("demo")"#);
    assert_eq!(a["gallery"][0]["title"], "Fixture demo");

    // And the module is served from the flat store at its hash.
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/modules/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/javascript; charset=utf-8"
    );
    // A content-addressed URL can never change meaning, so it is safe
    // to cache forever — and that is what lets the browser share one
    // module across instances without revalidating.
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        stored_bytes(body)
    );
}

/// The case the whole design exists for: two instances of one command
/// share a module and differ only in their gallery snippets.
#[tokio::test]
async fn two_instances_share_one_module_and_own_their_snippets() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "export default (id) => (root, ctx) => () => {};";
    let hash = sha256_hex(stored_bytes(body).as_bytes());
    let script = write_fixture(tmp.path(), "fixture.sh", body, &hash, &hash);

    let cfg = format!(
        r#"
[[applets]]
id = "a"
command = "sh {p}"

[[applets]]
id = "b"
command = "sh {p}"
"#,
        p = script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, applets) = get_json(&app, "/api/applets").await;
    let list = applets.as_array().unwrap();
    assert_eq!(list.len(), 2);

    // Same code: one URL, which is what makes the browser evaluate the
    // module once for both.
    assert_eq!(list[0]["components"]["view"], list[1]["components"]["view"]);
    // Distinct bindings: each snippet addresses its own instance.
    assert_eq!(list[0]["gallery"][0]["source"], r#"a.view("a")"#);
    assert_eq!(list[1]["gallery"][0]["source"], r#"b.view("b")"#);

    // One file on disk, written twice with identical bytes.
    let store: Vec<_> = std::fs::read_dir(datalib_http::applets::module_store_dir(tmp.path()))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != "CACHEDIR.TAG")
        .collect();
    assert_eq!(store.len(), 1, "expected exactly one shared module");
    assert_eq!(store[0].file_name().to_string_lossy(), hash);
}

/// A manifest that names a hash its bytes do not produce is a build
/// bug. Catching it at discovery beats serving stale code forever from
/// an immutable URL, which is what would happen otherwise.
#[tokio::test]
async fn rejects_a_module_whose_bytes_do_not_match_its_name() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "export default 1;";
    // Write real bytes under a name that does not describe them: the
    // file is present and readable, so only re-hashing catches it.
    let lie = "0".repeat(64);
    let script = write_fixture(tmp.path(), "liar.sh", body, &lie, &lie);

    let cfg = format!(
        "[[applets]]\nid = \"liar\"\ncommand = \"sh {}\"\n",
        script.display()
    );
    let app = router(state_with(tmp.path(), &cfg).await);
    let (_, applets) = get_json(&app, "/api/applets").await;
    let a = &applets.as_array().unwrap()[0];
    let err = a["error"].as_str().expect("expected a discovery error");
    assert!(
        err.contains("hash to"),
        "expected a digest-mismatch error, got: {err}"
    );
    assert!(a["components"].as_object().unwrap().is_empty());
}

/// A broken applet must not take the others down with it, and must
/// stay visible — a configured applet that silently vanished would
/// look like a config that never saved.
#[tokio::test]
async fn a_failing_applet_is_reported_without_hiding_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "export default 2;";
    let hash = sha256_hex(stored_bytes(body).as_bytes());
    let good = write_fixture(tmp.path(), "good.sh", body, &hash, &hash);
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
    let (_, applets) = get_json(&app, "/api/applets").await;
    let list = applets.as_array().unwrap();

    let ok = list.iter().find(|a| a["id"] == "ok").unwrap();
    assert!(ok.get("error").is_none());
    assert_eq!(ok["components"]["view"], hash);

    let broken = list.iter().find(|a| a["id"] == "broken").unwrap();
    let err = broken["error"].as_str().unwrap();
    // The child's stderr is what tells the user what to fix.
    assert!(err.contains("boom"), "stderr not surfaced: {err}");
}

#[tokio::test]
async fn module_paths_cannot_escape_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);

    // Anything that is not a lowercase 64-hex digest is refused before
    // the name is ever joined onto the store directory.
    for bad in [
        "not-a-hash".to_string(),
        "A".repeat(64), // uppercase: outside the accepted alphabet
        "a".repeat(63), // too short
        "a".repeat(64), // well-formed but absent
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/modules/{bad}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "/modules/{bad}");
    }

    // A path with separators does not match the single-segment route
    // at all, so it lands on the SPA fallback. That answers 200 with
    // index.html by design; what matters is that it is the app shell
    // and not a file read off the disk.
    let resp = app
        .oneshot(
            Request::get("/modules/../../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        !ctype.starts_with("text/javascript"),
        "traversal reached the module store: {ctype}"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        !body.contains("root:"),
        "served something off the filesystem"
    );
}

#[tokio::test]
async fn no_applets_configured_is_an_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    let (status, applets) = get_json(&app, "/api/applets").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(applets, serde_json::json!([]));
}

/// A request for an applet that is not configured is a gateway error
/// carrying a reason, not a hang and not a 404 the card would render
/// as "no data".
#[tokio::test]
async fn proxying_an_unknown_applet_reports_why() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state_with(tmp.path(), "").await);
    let resp = app
        .oneshot(
            Request::get("/v/nope/channels")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("nope"),
        "error should name the applet: {v}"
    );
}
