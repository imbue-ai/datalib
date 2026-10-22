//! `GET /data/<group>/<function>/blob/<hash>[/part/<hash>]` — a step's
//! blob store row, or a part of an email stored there, by the address a
//! page links it with.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use datalib_http::router;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

const CONFIG: &str = r#"
[[groups]]
id = "pdfs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"
"#;

/// Writes `<tree>/blobs.sqlite` the way the contract says a step does,
/// and answers each row's hash.
async fn blob_store(tree: &Path, rows: &[(Option<&str>, &[u8])]) -> Vec<String> {
    std::fs::create_dir_all(tree).unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(tree.join(datalib_blob_store::BLOBS_DB))
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::query(datalib_blob_store::CAS_OBJECTS_DDL)
        .execute(&pool)
        .await
        .unwrap();
    let mut hashes = Vec::new();
    for (content_type, bytes) in rows {
        let hash = blake3::hash(bytes).to_hex().to_string();
        sqlx::query(
            "INSERT INTO cas_objects (blake3, byte_len, content_type, bytes) VALUES (?, ?, ?, ?)",
        )
        .bind(&hash)
        .bind(bytes.len() as i64)
        .bind(content_type)
        .bind(*bytes)
        .execute(&pool)
        .await
        .unwrap();
        hashes.push(hash);
    }
    pool.close().await;
    hashes
}

async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

fn header_of(resp: &axum::response::Response, name: header::HeaderName) -> Option<&str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// A row comes back as its bytes and type, for the browser to keep: an
/// address never names other bytes.
#[tokio::test]
async fn a_steps_blob_comes_back_by_its_address() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[(Some("application/pdf"), b"%PDF-1.7 captain's log")],
    )
    .await;
    let app = router(state(td.path()).await);

    let resp = get(
        &app,
        &datalib_blob_store::address("pdfs/ingest", &hashes[0]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header_of(&resp, header::CONTENT_TYPE),
        Some("application/pdf")
    );
    assert_eq!(
        header_of(&resp, header::CACHE_CONTROL),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(
        header_of(&resp, header::X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"%PDF-1.7 captain's log");
}

/// A render-only source declares no `ingest` step, but its pages still
/// link the blob store in its `ingest` tree; the group being the
/// config's is what lets the route read it.
#[tokio::test]
async fn a_render_only_sources_blobs_are_served() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(
        td.path().join("config.toml"),
        r#"
[[groups]]
id = "pdfs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "render_markdown"
"#,
    )
    .unwrap();
    let hashes = blob_store(&td.path().join("pdfs/ingest"), &[(None, b"kept")]).await;
    let app = router(state(td.path()).await);

    let resp = get(
        &app,
        &datalib_blob_store::address("pdfs/ingest", &hashes[0]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The path is looked up, never opened as given: a store under a tree
/// that is not one of the config's groups, a kind this build does not
/// know, and a key that is not a hash are all "not found".
#[tokio::test]
async fn only_a_groups_store_is_read() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let kept = blob_store(&td.path().join("pdfs/ingest"), &[(None, b"kept")]).await;
    let stray = blob_store(&td.path().join("system/runs"), &[(None, b"stray")]).await;
    let app = router(state(td.path()).await);

    for uri in [
        datalib_blob_store::address("system/runs", &stray[0]),
        datalib_blob_store::address("pdfs/render_markdown", &kept[0]),
        format!("/data/pdfs/..%2E/blob/{}", kept[0]),
        format!("/data/pdfs/ingest/bytes/{}", kept[0]),
        format!("/data/pdfs/ingest/blob/{}", kept[0].to_uppercase()),
        "/data/pdfs/ingest/blob/..%2F..%2Fsystem%2Fapi-token".to_string(),
        datalib_blob_store::address("pdfs/ingest", &blake3::hash(b"never").to_hex()),
    ] {
        assert_eq!(
            get(&app, &uri).await.status(),
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

/// An attachment is whatever a sender wrote: an HTML page or an SVG gets
/// the sandbox that runs nothing, and script goes out as plain bytes.
#[tokio::test]
async fn an_attachment_cannot_run_in_the_apps_origin() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[
            (Some("text/html"), b"<script>alert(1)</script>"),
            (Some("application/javascript"), b"alert(1)"),
        ],
    )
    .await;
    let app = router(state(td.path()).await);

    let html = get(
        &app,
        &datalib_blob_store::address("pdfs/ingest", &hashes[0]),
    )
    .await;
    assert_eq!(html.status(), StatusCode::OK);
    assert!(
        header_of(&html, header::CONTENT_SECURITY_POLICY).is_some_and(|c| c.starts_with("sandbox")),
        "{:?}",
        html.headers()
    );
    let js = get(
        &app,
        &datalib_blob_store::address("pdfs/ingest", &hashes[1]),
    )
    .await;
    assert_eq!(
        header_of(&js, header::CONTENT_TYPE),
        Some("application/octet-stream")
    );
}

const EML: &[u8] = b"From: Beverly Crusher <crusher@enterprise.example>\r\n\
To: Jean-Luc Picard <picard@enterprise.example>\r\n\
Subject: Scan results\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<script>alert(1)</script>\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-Disposition: attachment; filename=\"scan.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--b--\r\n";

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

fn hash_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A part of a stored email comes back by the hash of its decoded bytes,
/// typed as the email says, with the answer headers a row gets.
#[tokio::test]
async fn a_part_of_a_stored_email_comes_back_with_its_type() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[(Some("message/rfc822"), EML)],
    )
    .await;
    let app = router(state(td.path()).await);

    let resp = get(
        &app,
        &datalib_blob_store::part_address("pdfs/ingest", &hashes[0], &hash_of(PNG)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_of(&resp, header::CONTENT_TYPE), Some("image/png"));
    assert_eq!(
        header_of(&resp, header::CACHE_CONTROL),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(
        header_of(&resp, header::X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], PNG);
}

/// A part the email does not have, a row that is not an email though it
/// holds one's bytes, and anything after the hash but `part/<hash>` are
/// all "not found".
#[tokio::test]
async fn only_a_part_an_email_has_is_found() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    // An email's bytes typed as a PDF; one byte longer, so its own row.
    let not_an_email = [EML, b"\r\n"].concat();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[
            (Some("message/rfc822"), EML),
            (Some("application/pdf"), &not_an_email),
        ],
    )
    .await;
    let app = router(state(td.path()).await);
    let (eml, pdf, png) = (&hashes[0], &hashes[1], hash_of(PNG));

    for uri in [
        datalib_blob_store::part_address("pdfs/ingest", eml, &hash_of(b"never")),
        datalib_blob_store::part_address("pdfs/ingest", pdf, &png),
        format!("/data/pdfs/ingest/blob/{eml}/part"),
        format!("/data/pdfs/ingest/blob/{eml}/part/"),
        format!("/data/pdfs/ingest/blob/{eml}/parts/{png}"),
        format!("/data/pdfs/ingest/blob/{eml}/part/{png}/"),
        format!("/data/pdfs/ingest/blob/{eml}/part/{png}/part/{png}"),
        format!("/data/pdfs/ingest/blob/{eml}/part/{}", png.to_uppercase()),
        format!("/data/pdfs/ingest/blob/{eml}/part/..%2F..%2Fsystem%2Fapi-token"),
        format!("/data/pdfs/ingest/blob/{}/part/{png}", &eml[1..]),
    ] {
        assert_eq!(
            get(&app, &uri).await.status(),
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

/// An HTML part is whatever its sender wrote: it gets the sandbox that
/// runs nothing, as an HTML row does.
#[tokio::test]
async fn an_html_part_cannot_run_in_the_apps_origin() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[(Some("message/rfc822"), EML)],
    )
    .await;
    let app = router(state(td.path()).await);

    let html = get(
        &app,
        &datalib_blob_store::part_address(
            "pdfs/ingest",
            &hashes[0],
            &hash_of(b"<script>alert(1)</script>"),
        ),
    )
    .await;
    assert_eq!(html.status(), StatusCode::OK);
    assert_eq!(
        header_of(&html, header::CONTENT_TYPE),
        Some("text/html; charset=utf-8")
    );
    assert!(
        header_of(&html, header::CONTENT_SECURITY_POLICY).is_some_and(|c| c.starts_with("sandbox")),
        "{:?}",
        html.headers()
    );
}
