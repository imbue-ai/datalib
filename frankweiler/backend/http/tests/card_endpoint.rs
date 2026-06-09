//! End-to-end integration test for `POST /api/card` + `GET /api/card/{hash}`.
//!
//! Opens a doltlite file in a temp directory (the AppState contract
//! requires a repo even though these endpoints don't touch SQL) and
//! drives requests through `tower::ServiceExt::oneshot`. Verifies:
//!   - POST returns a 64-char hex hash.
//!   - GET round-trips the JS body verbatim.
//!   - Re-POSTing the same source returns the same hash.
//!   - GET on an unknown hash → 404, on a malformed hash → 400.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("fw-http-card-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db")
}

#[tokio::test]
async fn card_round_trip_and_error_paths() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let dolt = DoltRepo::open(&db_path, root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));
    let app_state = AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        qmd_daemon: None,
    };
    let app = router(app_state.clone());

    // Round trip a non-trivial body (includes characters the SFC parser
    // chokes on — `<script>`/`</script>` — so we incidentally verify the
    // backend treats the JS as bytes, not as HTML).
    let source = "const x = 1; document.body.innerHTML = '<script>alert(1)</script>';";
    let req = Request::post("/api/card")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"source": source})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let hash = parsed["hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 64, "sha256 hex is 64 chars, got {hash:?}");
    assert!(
        hash.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "hash should be lowercase hex"
    );

    // GET should round-trip the source verbatim.
    let req = Request::get(format!("/api/card/{hash}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(std::str::from_utf8(&body).unwrap(), source);

    // Re-POSTing the same source is idempotent — same hash, file untouched.
    let stored = root.join(".frankweiler/cards").join(format!("{hash}.js"));
    let mtime_before = std::fs::metadata(&stored).unwrap().modified().unwrap();
    let req = Request::post("/api/card")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"source": source})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["hash"].as_str().unwrap(), hash);
    let mtime_after = std::fs::metadata(&stored).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "duplicate POST should not rewrite the file"
    );

    // Unknown hash (valid shape, no file) → 404.
    let bogus = "0".repeat(64);
    let req = Request::get(format!("/api/card/{bogus}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Malformed hash (wrong length, non-hex char including uppercase
    // since lowercase-hex is the contract) → 400. Path-traversal
    // attempts like `../../etc/passwd` contain `/` and so never reach
    // this handler — axum's router doesn't match `{hash}` against a
    // multi-segment path. The defensive hex check here is belt-and-
    // suspenders in case a future routing change relaxes that.
    let upper = "G".repeat(64);
    for bad in ["short", upper.as_str()] {
        let req = Request::get(format!("/api/card/{bad}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad:?}, got {:?}",
            resp.status()
        );
    }

    drop(app_state);
    let _ = std::fs::remove_file(&db_path);
}
