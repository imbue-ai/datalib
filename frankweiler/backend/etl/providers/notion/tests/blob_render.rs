//! End-to-end: a Notion image block whose bytes live in the doltlite
//! `blobs` table renders into a markdown file with a relative link, and
//! the bytes land next to the .md so the link resolves.

use std::collections::HashMap;
use std::fs;

use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl_notion::render_and_index_md::parse::ParsedNotionOfficial;
use frankweiler_etl_notion::render_and_index_md::render::render_notion_official;
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

    let blake3 = frankweiler_etl::blob_cas::blake3_hex(&bytes);
    let mut bundle = BlobBundle::new();
    bundle.add(
        format!("{bid}:image"),
        bytes.clone(),
        Some("image/png".into()),
        None,
    );
    let mut blobs_by_page = HashMap::new();
    blobs_by_page.insert(pid.to_string(), bundle);

    let parsed = ParsedNotionOfficial {
        pages: vec![page],
        blocks: vec![image_block],
        comments: vec![],
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_page,
    };

    let summary = render_notion_official(
        &parsed,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )
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

    // Hash-based filename via Blob::rendered_filename: first 16 hex
    // chars of blake3 + content-type extension.
    let short = &blake3[..16];
    let blob_path = page_dir.join("blobs").join(format!("{short}.png"));
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
        blobs_by_page: HashMap::new(),
    };

    render_notion_official(
        &parsed,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )
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

/// Regression test for the live-Notion page
/// `364a550f-af95-80de-829f-c5fccb3021fd` (Project Data Liberation
/// test page), where an image block rendered as the `*(image: image)*`
/// fallback instead of a real markdown image link.
///
/// The reproducer is a `file_upload`-typed image block — same shape
/// the Notion API returns for images that users upload through the
/// browser UI. That payload carries no `external.url` and no
/// `file.url`; without those, our render's `media_url` helper returns
/// empty, and (in the buggy state) we fall through to the
/// `*(image: …)*` placeholder.
///
/// This test FAILS today: it asserts the rendered md contains a real
/// `![…](…)` image link and not the placeholder. The fix likely lives
/// in the notion extractor (handle `file_upload`-typed image blocks
/// by fetching their bytes the same way `external`/`file` blocks are
/// already handled), at which point this assertion starts passing.
///
/// Marked `#[ignore]` so the rest of the suite stays green; run with
/// `cargo test -- --ignored` (or `bazelisk test
/// //frankweiler/backend/etl/providers/notion:notion_blob_render
/// --test_arg=--ignored`) to see the actual failure.
#[test]
#[ignore = "BUG: image renders as *(image: image)* fallback for file_upload-typed blocks"]
fn file_upload_image_renders_as_real_image_not_fallback() {
    let d = tempdir().unwrap();
    let root = d.path();

    let pid = "33333333-4444-5555-6666-777777777777";
    let bid = "cccccccc-dddd-eeee-ffff-000000000000";

    let page = json!({
        "id": pid,
        "object": "page",
        "parent": {"type": "workspace"},
        "properties": {"title": {"title": [{"plain_text": "uploaded image page"}]}},
        "created_time": "2026-05-27T00:00:00.000Z",
        "last_edited_time": "2026-05-27T00:00:00.000Z",
    });
    // file_upload shape: no `file.url` / no `external.url` — same as
    // the live capture for the buggy page. The image is identified by
    // a server-side `file_upload.id`, and Notion expects clients to
    // resolve that to a URL out-of-band.
    let image_block = json!({
        "id": bid,
        "type": "image",
        "has_children": false,
        "parent": {"type": "page_id", "page_id": pid},
        "image": {
            "type": "file_upload",
            "file_upload": {"id": "55555555-6666-7777-8888-999999999999"},
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
        blobs_by_page: HashMap::new(),
    };

    render_notion_official(
        &parsed,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )
    .expect("render ok");

    let md = fs::read_to_string(
        root.join("rendered_md")
            .join("notion")
            .join("pages")
            .join(pid)
            .join("index.md"),
    )
    .expect("md exists");

    assert!(
        !md.contains("*(image:"),
        "image block rendered as the *(image: …)* placeholder; \
         expected a real ![…](…) link. Page md:\n{md}",
    );
    assert!(
        md.contains("!["),
        "expected an image markdown link in the rendered page; got:\n{md}",
    );
}

/// Incrementality canary for notion: two pages, render once to seed
/// the prior_fingerprints map, then mutate page B (add a paragraph
/// block) and render again. Only page B should re-render.
///
/// Mirrors the slack canary
/// (`renders_only_changed_and_new_threads_on_resync`) but for
/// notion's distinct doc shape — page-with-blocks rather than
/// thread-of-messages.
#[test]
fn incremental_renders_only_changed_page() {
    let d = tempdir().unwrap();
    let root = d.path();

    let page_a = "a0000000-1111-2222-3333-444444444444";
    let page_b = "b0000000-1111-2222-3333-444444444444";

    let make_paragraph = |id: &str, parent_page: &str, text: &str| {
        json!({
            "id": id,
            "type": "paragraph",
            "has_children": false,
            "parent": {"type": "page_id", "page_id": parent_page},
            "paragraph": {
                "rich_text": [{"type": "text", "text": {"content": text}, "plain_text": text}]
            }
        })
    };
    let make_page = |id: &str, title: &str| {
        json!({
            "id": id,
            "object": "page",
            "parent": {"type": "workspace"},
            "properties": {"title": {"title": [{"plain_text": title}]}},
            "created_time": "2026-05-01T00:00:00.000Z",
            "last_edited_time": "2026-05-01T00:00:00.000Z",
        })
    };

    // ── pass 1: two pages, one block each. Render with empty priors. ─
    let parsed_v1 = ParsedNotionOfficial {
        pages: vec![make_page(page_a, "page A"), make_page(page_b, "page B")],
        blocks: vec![
            make_paragraph(
                "10000000-aaaa-bbbb-cccc-dddddddddddd",
                page_a,
                "page A original body",
            ),
            make_paragraph(
                "20000000-aaaa-bbbb-cccc-dddddddddddd",
                page_b,
                "page B original body",
            ),
        ],
        comments: vec![],
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_page: HashMap::new(),
    };

    let mut priors: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let summary1 = render_notion_official(
        &parsed_v1,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |doc: frankweiler_etl::load::RenderedMarkdown| -> anyhow::Result<()> {
            priors.insert(doc.markdown_uuid.clone(), doc.source_fingerprint.clone());
            Ok(())
        },
    )
    .expect("render v1");
    assert_eq!(summary1.rendered, 2);
    assert_eq!(summary1.skipped, 0);
    assert!(priors.contains_key(page_a));
    assert!(priors.contains_key(page_b));
    let fp_a_v1 = priors.get(page_a).cloned().unwrap();

    // ── mutate: extend page B with a second paragraph; page A
    //           untouched. ─────────────────────────────────────────────
    let parsed_v2 = ParsedNotionOfficial {
        pages: vec![make_page(page_a, "page A"), make_page(page_b, "page B")],
        blocks: vec![
            make_paragraph(
                "10000000-aaaa-bbbb-cccc-dddddddddddd",
                page_a,
                "page A original body",
            ),
            make_paragraph(
                "20000000-aaaa-bbbb-cccc-dddddddddddd",
                page_b,
                "page B original body",
            ),
            make_paragraph(
                "30000000-aaaa-bbbb-cccc-dddddddddddd",
                page_b,
                "page B newly added second paragraph",
            ),
        ],
        comments: vec![],
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_page: HashMap::new(),
    };

    // ── pass 2: render with v1 priors; only page B should fire. ─────
    let mut rendered_uuids: Vec<String> = Vec::new();
    let mut captured_fp: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let summary2 = render_notion_official(
        &parsed_v2,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
        &mut |doc: frankweiler_etl::load::RenderedMarkdown| -> anyhow::Result<()> {
            rendered_uuids.push(doc.markdown_uuid.clone());
            captured_fp.insert(doc.markdown_uuid.clone(), doc.source_fingerprint.clone());
            Ok(())
        },
    )
    .expect("render v2");
    assert_eq!(
        rendered_uuids,
        vec![page_b.to_string()],
        "expected only page B to re-render; got {rendered_uuids:?}",
    );
    assert_eq!(summary2.rendered, 1);
    assert_eq!(summary2.skipped, 1);

    // Page A's fingerprint must be unchanged (the indexer relied on
    // that to skip it); page B's must differ.
    assert_eq!(
        priors.get(page_a),
        Some(&fp_a_v1),
        "page A's fingerprint must not have drifted",
    );
    let fp_b_v2 = captured_fp.get(page_b).expect("page B in capture");
    assert_ne!(
        priors.get(page_b).unwrap(),
        fp_b_v2,
        "page B's fingerprint should change when its blocks change",
    );

    // ── disk: page B's md now contains the new paragraph text. ──────
    let page_b_md = fs::read_to_string(
        root.join("rendered_md")
            .join("notion")
            .join("pages")
            .join(page_b)
            .join("index.md"),
    )
    .expect("page B md exists");
    assert!(
        page_b_md.contains("page B newly added second paragraph"),
        "page B's md should include the appended paragraph; got:\n{page_b_md}",
    );

    // ── pass 3: feed v2's priors back; nothing should re-render. ────
    let mut priors_v2 = priors.clone();
    priors_v2.extend(captured_fp);
    let summary3 = render_notion_official(
        &parsed_v2,
        root,
        &frankweiler_etl::progress::Progress::noop(),
        &priors_v2,
        &mut |_doc| panic!("steady state should skip every doc"),
    )
    .expect("render v3");
    assert_eq!(summary3.rendered, 0);
    assert_eq!(summary3.skipped, 2);
}
