//! Render-side attachment behaviour, and the incrementality canary.

use std::collections::{HashMap, HashSet};
use std::fs;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_notion::render::parse::ParsedNotion;
use datalib_etl_notion::render::render::render_notion;
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

/// Returns `(emitted docs, every document the render considered)`. The
/// second is what the driver sweeps against, and it must include
/// documents skipped on an unchanged fingerprint.
fn render(
    parsed: &ParsedNotion,
    root: &std::path::Path,
    prior: &HashMap<String, String>,
) -> (Vec<(String, String)>, HashSet<String>) {
    let mut emitted = Vec::new();
    let mut considered: HashSet<String> = HashSet::new();
    {
        let mut on_doc = |md: datalib_etl::grid_index::RenderedMarkdown| {
            emitted.push((md.markdown_uuid.clone(), md.source_fingerprint.clone()));
            Ok(())
        };
        render_notion(
            parsed,
            root,
            "notion",
            &Progress::noop(),
            prior,
            &mut on_doc,
            &mut considered,
        )
        .unwrap();
    }
    (emitted, considered)
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
    render(&parsed, d.path(), &HashMap::new());

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
    render(&parsed, d.path(), &HashMap::new());
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
    render(&parsed, d.path(), &HashMap::new());
    let dir = fs::read_dir(d.path().join("notion/rendered_md/pages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let md = fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(md.contains("| Status | Active |"), "{md}");
}

/// Incrementality canary. Render two pages, then change one body and
/// render again: only the changed page comes back.
///
/// This is the property the slot rewrite exists to protect — if signed
/// URLs reached the stored markdown, every page with an attachment
/// would re-render on every run and this test would catch it.
#[test]
fn only_the_changed_page_re_renders() {
    let d = tempdir().unwrap();
    let (a, b) = (
        "aaaaaaaa-2222-3333-4444-555555555555",
        "bbbbbbbb-2222-3333-4444-555555555555",
    );
    let mut parsed = ParsedNotion {
        pages: vec![page(a, "Page A"), page(b, "Page B")],
        markdown_by_page: [
            (a.to_string(), "# A\n".to_string()),
            (b.to_string(), "# B\n".to_string()),
        ]
        .into_iter()
        .collect(),
        comments: vec![],
        blobs_by_page: HashMap::new(),
        ..Default::default()
    };
    let (first, considered) = render(&parsed, d.path(), &HashMap::new());
    assert_eq!(first.len(), 2);
    assert_eq!(considered.len(), 2, "both pages were considered");
    let prior: HashMap<String, String> = first.into_iter().collect();

    // Unchanged input: nothing re-renders...
    let (emitted, considered) = render(&parsed, d.path(), &prior);
    assert!(
        emitted.is_empty(),
        "an unchanged tree must produce no documents"
    );
    // ...but both pages must still be *named*. A renderer that reported
    // only what it re-rendered would tell the driver its whole steady
    // state had gone upstream, and the sweep would delete it.
    assert_eq!(
        considered.len(),
        2,
        "skipped documents must still be reported as present"
    );

    // Change B's body only.
    parsed
        .markdown_by_page
        .insert(b.to_string(), "# B\n\nnew paragraph\n".into());
    let (second, _) = render(&parsed, d.path(), &prior);
    let ids: Vec<&str> = second.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec![b], "only the changed page should re-render");
}
