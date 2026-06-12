//! Golden test for ChatGPT translate::render against the TNG fixture.
//!
//! The expected snapshot is byte-equal to what `src/ingest/render.py`
//! produces for the same fixture; the .snap was seeded from a Python
//! render pass and the Rust port is expected to converge on it.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use frankweiler_etl_chatgpt::translate::parse::parse_api_dir;
use frankweiler_etl_chatgpt::translate::render::render_all;

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
    render_all(
        &parsed,
        tmp.path(),
        "chatgpt_api",
        &frankweiler_etl::progress::Progress::noop(),
        &mut |_doc| Ok(()),
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

    let sidecars = collect_by_ext(tmp.path(), ".grid_rows.json");
    let mut sbundle = String::new();
    for (path, body) in &sidecars {
        sbundle.push_str("=== ");
        sbundle.push_str(path);
        sbundle.push_str(" ===\n");
        sbundle.push_str(body);
        sbundle.push('\n');
    }
    insta::assert_snapshot!("tng_sidecar_tree", sbundle);
}
