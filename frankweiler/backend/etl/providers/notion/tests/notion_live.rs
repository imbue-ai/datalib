//! Live Notion single-page download test.
//!
//! Hits real `api.notion.com/v1` via `latchkey curl`, downloads ONE
//! page (its body + comments) into a hermetic tempdir, and
//! insta-snapshots a curated stable view of what came back. Serves as
//! both an integration smoke test and a piece of documentation
//! showing what a Notion page capture looks like end-to-end.
//!
//! The default target is the imbue-ai
//! "Project Data Liberation — test page" — a stable Notion page kept
//! for this test. Override with `NOTION_TEST_PAGE=<uuid>` to point at
//! a different page (UUID, dashed or undashed).
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo. Run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim
//! cargo test -p frankweiler-etl-notion --test notion_live -- --ignored
//! ```

use std::fs;

use frankweiler_etl_notion::extract::{self as notion, FetchOptions};
use frankweiler_etl_notion::translate::parse_api_dir;
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

    let tmp = std::env::temp_dir().join(format!(
        "notion-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    eprintln!("[test] downloading {page} -> {}", tmp.display());

    let opts = FetchOptions {
        out_dir: tmp.clone(),
        page: Some(page.clone()),
        ..Default::default()
    };
    notion::fetch(opts).await.expect("notion fetch failed");

    let parsed = parse_api_dir(&tmp).expect("parse_api_dir");
    assert_eq!(parsed.pages.len(), 1, "expected exactly one page");

    let p = &parsed.pages[0];
    let mut block_kinds: Vec<String> = parsed
        .blocks
        .iter()
        .filter_map(|b| b.get("type").and_then(|v| v.as_str()).map(String::from))
        .collect();
    block_kinds.sort();
    block_kinds.dedup();
    let view = json!({
        "object": p.get("object"),
        "has_id": p.get("id").and_then(|v| v.as_str()).is_some(),
        "parent_kind": p.get("parent").and_then(|v| v.get("type")),
        "archived": p.get("archived"),
        "block_count": parsed.blocks.len(),
        "block_kinds": block_kinds,
        "comment_count": parsed.comments.len(),
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!("notion_live_single_page_snapshot", view);
    });
}
