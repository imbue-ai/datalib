//! Golden test for `slack::render` against the TNG fixture.
//!
//! Renders the fixture into a tempdir, snapshots the per-thread `.md`
//! payloads. Incrementality (dolt_diff-driven skip) is exercised by
//! `slack_incremental.rs` against a real doltlite DB.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use frankweiler_etl_slack::translate::{parse, render::render_all};
use insta::assert_snapshot;

fn fixture_root() -> PathBuf {
    if let Ok(d) = std::env::var("SLACK_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api")
}

fn collect_md(root: &std::path::Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                out.insert(rel, fs::read_to_string(&p).unwrap());
            }
        }
    }
    walk(root, root, &mut out);
    out
}

#[test]
fn renders_tng_fixture() {
    let parsed = parse(&fixture_root(), None).expect("parse");
    let tmp = tempfile::tempdir().expect("tmp");
    let mut completed = 0usize;
    let mut on_done = |_doc: frankweiler_etl::load::RenderedMarkdown| -> anyhow::Result<()> {
        completed += 1;
        Ok(())
    };
    let summary = render_all(
        &parsed,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &mut on_done,
    )
    .expect("render");
    assert_eq!(summary.threads_total, 6);
    assert_eq!(summary.threads_rendered, 6);
    assert_eq!(summary.threads_skipped, 0);
    assert_eq!(completed, 6);

    let md_tree = collect_md(tmp.path());
    let mut bundle = String::new();
    for (path, body) in &md_tree {
        bundle.push_str("=== ");
        bundle.push_str(path);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    assert_snapshot!("tng_md_tree", bundle);

    // Sidecar shape spot-check.
    let mut sidecar_paths: Vec<PathBuf> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.to_string_lossy().ends_with(".grid_rows.json") {
                out.push(p);
            }
        }
    }
    walk(tmp.path(), &mut sidecar_paths);
    assert_eq!(sidecar_paths.len(), 6);
    let one: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&sidecar_paths[0]).unwrap()).unwrap();
    assert!(one
        .get("header")
        .and_then(|h| h.get("source_fingerprint"))
        .is_some());
    assert!(one.get("rows").and_then(|r| r.as_array()).is_some());
}
