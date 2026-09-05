//! Golden test for ChatGPT render::render against the TNG fixture.
//!
//! The expected snapshot is byte-equal to what `src/ingest/render.py`
//! produces for the same fixture; the .snap was seeded from a Python
//! render pass and the Rust port is expected to converge on it.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use datalib_etl_chatgpt::render::parse::parse_api_dir;
use datalib_etl_chatgpt::render::render::render_all;

fn fixture_dir() -> PathBuf {
    // Bazel sets `CHATGPT_FIXTURE_DIR` to a runfiles-relative path
    // and stages the fixture there via `data = [":tng_fixture"]`.
    // Cargo's `CARGO_MANIFEST_DIR` is the package source on disk.
    if let Ok(d) = std::env::var("CHATGPT_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chatgpt_api")
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

#[test]
fn renders_tng_fixture() {
    let parsed = parse_api_dir(&fixture_dir()).expect("parse");
    let tmp = tempfile::tempdir().expect("tmp");
    let mut docs = Vec::new();
    render_all(
        &parsed,
        tmp.path(),
        "chatgpt_api",
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
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
}

/// The documents the renderer emitted, as a reviewable bundle.
///
/// Snapshots what `render_all` *produces* — the `RenderedMarkdown`
/// values it hands to its callback — rather than the
/// `*.grid_rows.json` files it used to also write. That serialization
/// is going away; the emitted document is the renderer's actual
/// contract, and it is what both the store and the unified index
/// consume.
///
/// Keyed by `markdown_uuid` and sorted, so the bundle is stable
/// regardless of the order documents happen to be rendered in.
fn docs_bundle(docs: &[datalib_etl::grid_index::RenderedMarkdown]) -> String {
    let mut sorted: Vec<&datalib_etl::grid_index::RenderedMarkdown> = docs.iter().collect();
    sorted.sort_by(|a, b| a.markdown_uuid.cmp(&b.markdown_uuid));
    let mut out = String::new();
    for d in sorted {
        out.push_str("=== ");
        out.push_str(&d.markdown_uuid);
        out.push_str(" ===\n");
        let v = serde_json::json!({
            "source_fingerprint": d.source_fingerprint,
            "render_version": d.render_version,
            "rows": d.rows,
            "edges": d.edges,
        });
        out.push_str(&serde_json::to_string_pretty(&v).unwrap());
        out.push('\n');
    }
    out
}
