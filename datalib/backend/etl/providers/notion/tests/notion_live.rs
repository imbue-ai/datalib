// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Notion single-page download test.

use datalib_etl_notion::download::{self as notion, FetchOptions};
use datalib_etl_notion::render::parse_api_dir;
use insta::assert_json_snapshot;
use serde_json::json;

const DEFAULT_TARGET_PAGE: &str = "364a550f-af95-80de-829f-c5fccb3021fd";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn notion_live_single_page_snapshot() {
    let page = std::env::var("NOTION_TEST_PAGE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TARGET_PAGE.to_string());

    let tmp = tempfile::TempDir::with_prefix("notion-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading {page} -> {}", tmp.display());

    let opts = FetchOptions {
        db_path: tmp.clone(),
        page: Some(page.clone()),
        ..Default::default()
    };
    notion::fetch(opts).await.expect("notion fetch failed");

    let parsed = parse_api_dir(&tmp, None).expect("parse_api_dir");
    assert_eq!(parsed.pages.len(), 1, "expected exactly one page");

    let p = &parsed.pages[0];
    let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    let body = parsed.markdown_by_page.get(pid);
    // Deliberately *not* snapshotting the markdown itself or its
    // length: this runs against a live page whose text changes. What
    // must hold is that a body was stored at all, and — the property
    // the whole design rests on — that no signed URL survived into it.
    let view = json!({
        "object": p.get("object"),
        "has_id": !pid.is_empty(),
        "parent_kind": p.get("parent").and_then(|v| v.get("type")),
        "in_trash": p.get("in_trash"),
        "has_markdown": body.is_some(),
        "markdown_carries_no_signature": body
            .map(|m| !m.contains("X-Amz-Signature"))
            .unwrap_or(true),
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!("notion_live_single_page_snapshot", view);
    });
}
