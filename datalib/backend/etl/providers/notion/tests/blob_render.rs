//! Render-side attachment behaviour, and the incrementality canary.

use std::collections::HashMap;
use std::fs;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_notion_render::render::parse::ParsedNotion;
use datalib_etl_notion_render::render::render::render_notion;
use serde_json::json;
use tempfile::tempdir;

const SLOT: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/8b76f77e/image.png";

fn page(id: &str, title: &str) -> serde_json::Value {
    json!({
        "id": id,
        "object": "page",
        "parent": {"type": "workspace"},
        "properties": {"title": {"type": "title", "title": [{"plain_text": title}]}},
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": "2026-01-01T00:00:00.000Z",
    })
}

fn render(parsed: &ParsedNotion, root: &std::path::Path) -> Vec<String> {
    let mut emitted = Vec::new();
    {
        let mut on_doc = |md: datalib_etl_render::grid_index::RenderedMarkdown| {
            emitted.push(md.markdown_uuid.clone());
            Ok(())
        };
        render_notion(parsed, root, "notion", &Progress::noop(), &mut on_doc).unwrap();
    }
    emitted
}

/// An attachment whose bytes are in the CAS is written beside the page
/// and the body points at the local file, not at the upstream slot.
#[test]
fn an_archived_attachment_is_linked_locally() {
    let d = tempdir().unwrap();
    let pid = "11111111-2222-3333-4444-555555555555";
    let mut bundle = BlobBundle::new();
    bundle.add(
        SLOT,
        b"\x89PNG\r\n\x1a\nfake".to_vec(),
        Some("image/png".into()),
        None,
    );
    let mut blobs = HashMap::new();
    blobs.insert(pid.to_string(), bundle);

    let parsed = ParsedNotion {
        pages: vec![page(pid, "Test page")],
        markdown_by_page: [(pid.to_string(), format!("# Hi\n\n![shot]({SLOT})\n"))]
            .into_iter()
            .collect(),
        comments: vec![],
        blobs_by_page: blobs,
        ..Default::default()
    };
    render(&parsed, d.path());

    let dir = fs::read_dir(d.path().join("notion/rendered_md/pages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let md = fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(
        md.contains("![shot](blobs/"),
        "body should link the local copy: {md}"
    );
    assert!(!md.contains(SLOT), "upstream slot should be gone: {md}");
    assert!(dir.join("blobs").is_dir(), "bytes should be materialized");
}

/// A slot with no bytes in the CAS keeps its upstream URL. A broken
/// relative path would be strictly worse than a link that still works.
#[test]
fn an_unarchived_attachment_keeps_its_upstream_url() {
    let d = tempdir().unwrap();
    let pid = "22222222-2222-3333-4444-555555555555";
    let parsed = ParsedNotion {
        pages: vec![page(pid, "No blobs")],
        markdown_by_page: [(pid.to_string(), format!("![shot]({SLOT})\n"))]
            .into_iter()
            .collect(),
        comments: vec![],
        blobs_by_page: HashMap::new(),
        ..Default::default()
    };
    render(&parsed, d.path());
    let dir = fs::read_dir(d.path().join("notion/rendered_md/pages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let md = fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(md.contains(SLOT), "unarchived slot must survive: {md}");
}

/// A database row's properties are its content — most of a real
/// workspace has no body at all, so a body-less page must still render
/// something.
#[test]
fn a_body_less_page_still_renders() {
    let d = tempdir().unwrap();
    let pid = "33333333-2222-3333-4444-555555555555";
    let mut p = page(pid, "A row");
    p["properties"]["Status"] = json!({"type": "select", "select": {"name": "Active"}});
    let parsed = ParsedNotion {
        pages: vec![p],
        markdown_by_page: HashMap::new(),
        comments: vec![],
        blobs_by_page: HashMap::new(),
        ..Default::default()
    };
    render(&parsed, d.path());
    let dir = fs::read_dir(d.path().join("notion/rendered_md/pages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let md = fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(md.contains("| Status | Active |"), "{md}");
}
