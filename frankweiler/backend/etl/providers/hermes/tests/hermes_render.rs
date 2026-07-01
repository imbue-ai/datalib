//! Render golden for the Hermes provider against the synthetic export fixture.
//!
//! Proves that Hermes/OpenClaw export files (JSONL session export, JSON
//! snapshot, and generic OpenClaw-shaped records) parse into conversations and
//! render to Markdown + grid_rows.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use frankweiler_etl_hermes::render_and_index_md::parse::parse_export_dir;
use frankweiler_etl_hermes::render_and_index_md::render::render_all;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HERMES_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes_export")
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
fn renders_hermes_fixture() {
    let parsed = parse_export_dir(&fixture_dir()).expect("parse");
    // Three sessions: CLI chat, Telegram agent trace, OpenClaw-generic.
    assert_eq!(parsed.sessions.len(), 3, "expected 3 sessions");

    let tmp = tempfile::tempdir().expect("tmp");
    let priors = std::collections::HashMap::new();
    render_all(
        &parsed,
        tmp.path(),
        "hermes",
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
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
    insta::assert_snapshot!("hermes_md_tree", bundle);

    let sidecars = collect_by_ext(tmp.path(), ".grid_rows.json");
    let mut sbundle = String::new();
    for (path, body) in &sidecars {
        sbundle.push_str("=== ");
        sbundle.push_str(path);
        sbundle.push_str(" ===\n");
        sbundle.push_str(body);
        sbundle.push('\n');
    }
    insta::assert_snapshot!("hermes_sidecar_tree", sbundle);
}
