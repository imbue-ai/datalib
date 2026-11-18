//! Golden test for Claude render::render against the TNG fixture.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use datalib_etl_claude::ingest::export::{ingest, IngestOptions};
use datalib_etl_claude_render::render::parse::parse;
use datalib_etl_claude_render::render::render::render_all;
use datalib_etl_render::inputs::RawRange;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_export")
}

/// The fixture as a person would have it, unpacked: the tree bazel stages
/// is links, and an export's reader does not follow one.
pub(crate) fn unpacked_fixture() -> tempfile::TempDir {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), &dest).unwrap();
            }
        }
    }
    let unpacked = tempfile::tempdir().expect("a directory for the fixture");
    copy(&fixture_dir(), unpacked.path());
    unpacked
}

fn collect_by_ext(root: &std::path::Path, ext: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        ext: &str,
        out: &mut BTreeMap<String, String>,
    ) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, ext, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                if rel.ends_with(ext) {
                    out.insert(rel, fs::read_to_string(&p).unwrap());
                }
            }
        }
    }
    walk(root, root, ext, &mut out);
    out
}

async fn ingest_fixture(raw: &Path) {
    let db =
        datalib_etl_claude::ingest::RawDb::open(&datalib_etl_claude::ingest::db::db_path_for(raw))
            .await
            .expect("open raw store");
    let export = unpacked_fixture();
    ingest(IngestOptions {
        db: db.clone(),
        input_path: export.path().to_path_buf(),
        now: "2026-09-04T00:00:00-07:00".to_string(),
        when_deleted_upstream: Default::default(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await
    .expect("ingest the TNG export");

    // Commit what the ingest wrote, the way the processor's
    // `RawStoreSession` does in production. Render pins HEAD, so an
    // uncommitted row is invisible to it.
    //
    // On the handle the ingest already holds: reopening here would be a
    // second live connection to the store, and one of the two commits
    // would fail with `commit conflict`.
    datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test: claude ingest")
        .await
        .expect("commit the ingest");
    // Closed, not dropped: whatever reads this store next is a second
    // connection until this one is actually gone.
    db.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn renders_tng_fixture() {
    let raw = tempfile::tempdir().expect("raw");
    ingest_fixture(raw.path()).await;
    let parsed = parse(raw.path(), "claude_export", RawRange::cold()).expect("parse");
    let tmp = tempfile::tempdir().expect("tmp");
    let mut docs = Vec::new();
    render_all(
        &parsed,
        tmp.path(),
        "claude_export",
        Default::default(),
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
        // One conversation's attachments a batch.
        1,
    )
    .expect("render");

    let md = collect_by_ext(tmp.path(), ".md");
    let mut bundle = String::new();
    for (path, body) in &md {
        bundle.push_str("=== ");
        bundle.push_str(path);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    insta::assert_snapshot!("tng_md_tree", bundle);

    insta::assert_snapshot!("tng_rendered_docs", docs_bundle(&docs));

    // The `stellar_cartography` project carries no `created_at` /
    // `updated_at` — a real shape, found in the live manual-e2e corpus,
    // where a Claude project row rendered `created_at` as
    // `1970-01-01T00:00:00+00:00`. Until this fixture landed, no
    // checked-in record anywhere was undated, so the goldens agreed
    // with the bug and could not have caught it.
    const PROJECT: &str = "70000002-1701-4d00-8000-000000000702";
    let undated = docs
        .iter()
        .find(|d| {
            d.rows
                .iter()
                .any(|r| r.upstream_id.as_deref() == Some(PROJECT))
        })
        .expect("the undated project rendered a document");
    assert!(
        !undated.rows.is_empty(),
        "the undated project produced rows"
    );
    for r in &undated.rows {
        assert!(
            r.created_at.is_none(),
            "a project with no created_at/updated_at must leave created_at null, \
             never a fabricated epoch — got {:?} on kind={}",
            r.created_at,
            r.kind
        );
    }
}

fn docs_bundle(docs: &[datalib_etl_render::grid_index::RenderedMarkdown]) -> String {
    let mut sorted: Vec<&datalib_etl_render::grid_index::RenderedMarkdown> = docs.iter().collect();
    sorted.sort_by(|a, b| a.markdown_uuid.cmp(&b.markdown_uuid));
    let mut out = String::new();
    for d in sorted {
        out.push_str("=== ");
        out.push_str(&d.markdown_uuid);
        out.push_str(" ===\n");
        let v = serde_json::json!({
            "render_version": d.render_version,
            "rows": d.rows,
            "edges": d.edges,
        });
        out.push_str(&serde_json::to_string_pretty(&v).unwrap());
        out.push('\n');
    }
    out
}
