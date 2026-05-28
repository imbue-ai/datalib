//! End-to-end: a Notion image block whose bytes live in the doltlite
//! `blobs` table renders into a markdown file with a relative link, and
//! the bytes land next to the .md so the link resolves.

use std::collections::HashMap;
use std::fs;

use frankweiler_etl_notion::extract::db::BlobBytes;
use frankweiler_etl_notion::translate::parse::ParsedNotionOfficial;
use frankweiler_etl_notion::translate::render::render_notion_official;
use serde_json::{json, Value};
use tempfile::tempdir;

#[test]
fn image_blob_lands_next_to_markdown() {
    let d = tempdir().unwrap();
    let root = d.path();

    let pid = "11111111-2222-3333-4444-555555555555";
    let bid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let bytes = b"\x89PNG\r\n\x1a\nfake-image-bytes".to_vec();

    let page = json!({
        "id": pid,
        "object": "page",
        "parent": {"type": "workspace"},
        "properties": {
            "title": {"title": [{"plain_text": "Test page"}]}
        },
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": "2026-01-01T00:00:00.000Z",
    });
    let image_block = json!({
        "id": bid,
        "type": "image",
        "has_children": false,
        "parent": {"type": "page_id", "page_id": pid},
        "image": {
            "type": "file",
            "file": {"url": "https://s3.notion-static.com/foo/test.png?expiry=123"},
            "caption": [{"plain_text": "look at this"}]
        }
    });

    let mut blobs_by_owner: HashMap<String, BlobBytes> = HashMap::new();
    blobs_by_owner.insert(
        bid.to_string(),
        BlobBytes {
            id: format!("{bid}:image"),
            owning_id: bid.to_string(),
            slot: "image".into(),
            content_type: Some("image/png".into()),
            bytes: bytes.clone(),
            source_url: Some("https://s3.notion-static.com/foo/test.png?expiry=123".into()),
        },
    );

    let parsed = ParsedNotionOfficial {
        pages: vec![page],
        blocks: vec![image_block],
        comments: vec![],
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_owner,
    };

    let summary =
        render_notion_official(&parsed, root, &frankweiler_etl::progress::Progress::noop())
            .expect("render ok");
    assert_eq!(summary.rendered, 1);

    // Page dir is `pages/<page_id>/` per render's page_dir_segment.
    let page_dir = root
        .join("rendered_md")
        .join("notion")
        .join("pages")
        .join(pid);
    let md = fs::read_to_string(page_dir.join("index.md")).expect("md exists");
    assert!(
        md.contains("![look at this](blobs/"),
        "expected relative blob link, got:\n{md}"
    );

    let blob_path = page_dir.join("blobs").join(format!("{bid}.png"));
    let on_disk = fs::read(&blob_path).expect("blob file exists");
    assert_eq!(on_disk, bytes);
}

#[test]
fn missing_blob_falls_back_to_upstream_url() {
    let d = tempdir().unwrap();
    let root = d.path();

    let pid = "22222222-3333-4444-5555-666666666666";
    let bid = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";

    let page = json!({
        "id": pid,
        "object": "page",
        "parent": {"type": "workspace"},
        "properties": {"title": {"title": [{"plain_text": "p"}]}},
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": "2026-01-01T00:00:00.000Z",
    });
    let image_block = json!({
        "id": bid,
        "type": "image",
        "has_children": false,
        "parent": {"type": "page_id", "page_id": pid},
        "image": {
            "type": "external",
            "external": {"url": "https://example.com/foo.png"},
            "caption": []
        }
    });

    let parsed = ParsedNotionOfficial {
        pages: vec![page],
        blocks: vec![image_block],
        comments: vec![],
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_owner: HashMap::new(),
    };

    render_notion_official(&parsed, root, &frankweiler_etl::progress::Progress::noop())
        .expect("render ok");
    let md = fs::read_to_string(
        root.join("rendered_md")
            .join("notion")
            .join("pages")
            .join(pid)
            .join("index.md"),
    )
    .unwrap();
    assert!(
        md.contains("https://example.com/foo.png"),
        "expected fallback to upstream URL, got:\n{md}"
    );
    // And: no blobs dir was created (nothing to write).
    assert!(!root
        .join("rendered_md")
        .join("notion")
        .join("pages")
        .join(pid)
        .join("blobs")
        .exists());
    // Suppress unused-import warnings if any.
    let _ = Value::Null;
}
