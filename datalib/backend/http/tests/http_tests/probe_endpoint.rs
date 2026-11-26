//! `POST /api/probe` for a type a package offers: the package's own probe
//! command answers, with the wizard's params in `DATALIB_DAG_PARAMS`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::support::{get_json, state, TEST_TOKEN};

const SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

/// Prints a report whose account is the station the params name, and
/// whose one note is its own argv, so the test sees both what reached the
/// environment and that nothing secret reached the command line.
const PROBE: &str = r#"#!/bin/sh
case "$DATALIB_DAG_PARAMS" in
  *KSFO*) id=KSFO ;;
  *) id=none ;;
esac
printf '{"mode":"api","account":{"id":"%s","address":null,"display_name":null,"message_estimate":null},"items":[],"notes":["%s"]}\n' "$id" "$*"
"#;

fn install_weather(root: &Path, probe: bool) {
    let dir = root.join(package_dir_rel(SHA));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let script = dir.join("bin/probe-weather");
    std::fs::write(&script, PROBE).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let probe_line = if probe {
        "probe = \"bin/probe-weather\"\n"
    } else {
        ""
    };
    std::fs::write(
        dir.join(".datalib-package.toml"),
        format!(
            "schema = 1\nname = \"Weather\"\nversion = \"1.0.0\"\n\n\
             [artifacts]\nany = {{ url = \"w.tar.gz\", sha256 = \"{SHA}\" }}\n\n\
             [[sources]]\ntype = \"weather\"\n{probe_line}\
             commands = {{ ingest = \"bin/fetch-weather\" }}\n\
             [sources.wizard]\nlabel = \"Weather station\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(".datalib-installed"),
        datalib_packages::platform::this_target(),
    )
    .unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[[packages]]\nid = \"weather\"\nurl = \"https://example.com/datalib-package.toml\"\n\
             sha256 = \"{SHA}\"\n"
        ),
    )
    .unwrap();
}

fn package_dir_rel(sha: &str) -> String {
    format!("system/packages/{}", &sha[..16])
}

async fn post_probe(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/probe")
                .header("x-datalib-token", TEST_TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
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

#[tokio::test]
async fn a_packages_type_is_probed_by_the_packages_probe() {
    let tmp = tempfile::tempdir().unwrap();
    install_weather(tmp.path(), true);
    let app = datalib_http::router(state(tmp.path()).await);

    let (status, started) = post_probe(
        &app,
        serde_json::json!({
            "type": "weather",
            "params": { "station": "KSFO" },
            "list": "channels",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let id = started["id"].as_str().unwrap().to_string();

    let deadline = Instant::now() + Duration::from_secs(30);
    let done = loop {
        let (_, s) = get_json(&app, &format!("/api/probe/{id}")).await;
        if s["status"] != "running" {
            break s;
        }
        assert!(Instant::now() < deadline, "the probe never finished: {s}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(done["status"], "ok", "{done}");
    assert_eq!(done["report"]["account"]["id"], "KSFO", "{done}");
    assert_eq!(done["report"]["notes"][0], "--list channels", "{done}");
}

#[tokio::test]
async fn a_packages_type_with_no_probe_is_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    install_weather(tmp.path(), false);
    let app = datalib_http::router(state(tmp.path()).await);
    let (status, body) =
        post_probe(&app, serde_json::json!({ "type": "weather", "params": {} })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.to_string().contains("Weather"), "{body}");
}
