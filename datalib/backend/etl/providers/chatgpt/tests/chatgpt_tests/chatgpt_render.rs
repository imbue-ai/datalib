//! Golden test for ChatGPT render::render against the TNG fixture.

use std::collections::BTreeMap;
use std::path::PathBuf;

use datalib_etl_chatgpt_render::render::parse::parse_api_dir;
use datalib_etl_chatgpt_render::render::render::render_all;
use datalib_etl_render::section::join;

fn fixture_dir() -> PathBuf {
    // Bazel sets `CHATGPT_FIXTURE_DIR` to a runfiles-relative path
    // and stages the fixture there via `data = [":tng_fixture"]`.
    // Cargo's `CARGO_MANIFEST_DIR` is the package source on disk.
    if let Ok(d) = std::env::var("CHATGPT_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chatgpt_api")
}

#[test]
fn renders_tng_fixture() {
    let parsed = parse_api_dir(&fixture_dir(), "chatgpt_api").expect("parse");
    let mut docs = Vec::new();
    render_all(
        &parsed,
        "chatgpt_api",
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
        1,
    )
    .expect("render");

    let md: BTreeMap<&str, String> = docs
        .iter()
        .map(|d| (d.markdown_uuid.as_str(), join(&d.sections)))
        .collect();
    let mut bundle = String::new();
    for (uuid, body) in &md {
        bundle.push_str("=== ");
        bundle.push_str(uuid);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    insta::assert_snapshot!("tng_md_tree", bundle);

    insta::assert_snapshot!("tng_rendered_docs", docs_bundle(&docs));
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
