//! Render-side attachment behaviour, and the incrementality canary.

use std::collections::HashMap;

use datalib_etl::blob_cas::{BlobBundle, Described};
use datalib_etl::progress::Progress;
use datalib_etl_notion_render::render::parse::ParsedNotion;
use datalib_etl_notion_render::render::render::render_notion;
use datalib_etl_render::section::join;
use serde_json::json;

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

/// Renders `parsed` with `blobs` as the pages' bytes, one page a batch;
/// each document's markdown.
fn render(parsed: &ParsedNotion, blobs: &HashMap<String, BlobBundle>) -> Vec<String> {
    let mut emitted = Vec::new();
    {
        let mut on_doc = |md: datalib_etl_render::grid_index::RenderedMarkdown| {
            emitted.push(join(&md.sections));
            Ok(())
        };
        render_notion(parsed, "notion", &Progress::noop(), &mut on_doc, blobs, 1).unwrap();
    }
    emitted
}

/// An attachment whose bytes are in the CAS is linked at its address
/// there, not at the upstream slot.
#[test]
fn an_archived_attachment_is_linked_in_the_blob_store() {
    let pid = "11111111-2222-3333-4444-555555555555";
    let hash = "ab".repeat(32);
    let mut bundle = BlobBundle::new();
    bundle.add_stored(
        SLOT,
        "notion/ingest",
        Described {
            blake3: hash.clone(),
            byte_len: 12,
            content_type: Some("image/png".into()),
        },
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
        ..Default::default()
    };
    let md = &render(&parsed, &blobs)[0];
    assert!(
        md.contains(&format!("![shot](/data/notion/ingest/blob/{hash})")),
        "body should link the blob store: {md}"
    );
    assert!(!md.contains(SLOT), "upstream slot should be gone: {md}");
}

/// A slot with no bytes in the CAS keeps its upstream URL. A broken
/// relative path would be strictly worse than a link that still works.
#[test]
fn an_unarchived_attachment_keeps_its_upstream_url() {
    let pid = "22222222-2222-3333-4444-555555555555";
    let parsed = ParsedNotion {
        pages: vec![page(pid, "No blobs")],
        markdown_by_page: [(pid.to_string(), format!("![shot]({SLOT})\n"))]
            .into_iter()
            .collect(),
        comments: vec![],
        ..Default::default()
    };
    let md = &render(&parsed, &HashMap::new())[0];
    assert!(md.contains(SLOT), "unarchived slot must survive: {md}");
}

/// A database row's properties are its content — most of a real
/// workspace has no body at all, so a body-less page must still render
/// something.
#[test]
fn a_body_less_page_still_renders() {
    let pid = "33333333-2222-3333-4444-555555555555";
    let mut p = page(pid, "A row");
    p["properties"]["Status"] = json!({"type": "select", "select": {"name": "Active"}});
    let parsed = ParsedNotion {
        pages: vec![p],
        markdown_by_page: HashMap::new(),
        comments: vec![],
        ..Default::default()
    };
    let md = &render(&parsed, &HashMap::new())[0];
    assert!(md.contains("| Status | Active |"), "{md}");
}
