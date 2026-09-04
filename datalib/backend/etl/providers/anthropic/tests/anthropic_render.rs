//! Golden test for Anthropic render::render against the TNG fixture.
//!
//! The expected snapshot is byte-equal to what `src/ingest/render.py`
//! produces for the same fixture.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use datalib_etl_anthropic::render::parse::parse_export;
use datalib_etl_anthropic::render::render::render_all;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ANTHROPIC_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic_api")
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
    let parsed = parse_export(&fixture_dir()).expect("parse");
    let tmp = tempfile::tempdir().expect("tmp");
    render_all(
        &parsed,
        tmp.path(),
        "anthropic_api",
        Default::default(),
        &datalib_etl::progress::Progress::noop(),
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

    // The `stellar_cartography` project carries no `created_at` /
    // `updated_at` — a real shape, found in the live manual-e2e corpus,
    // where a Claude project row rendered `when_ts` as
    // `1970-01-01T00:00:00+00:00`. Until this fixture landed, no
    // checked-in record anywhere was undated, so the goldens agreed
    // with the bug and could not have caught it.
    //
    // Asserted explicitly rather than left to the snapshot above,
    // because a golden only says "this matches what it matched last
    // time" — someone re-baking it would carry a regression straight
    // through. See `docs/dev/data_architecture_parse_and_render.md` §6
    // and R5 in §4.
    // Found by the upstream project uuid in the row bodies, not by path:
    // the directory is named for the `markdown_uuid` the renderer mints,
    // not for the id upstream gave us.
    let undated = sidecars
        .values()
        .find(|body| body.contains("70000002-1701-4d00-8000-000000000702"))
        .expect("the undated project rendered a sidecar");
    let v: serde_json::Value = serde_json::from_str(undated).expect("sidecar parses");
    let rows = v["rows"].as_array().expect("rows array");
    assert!(!rows.is_empty(), "the undated project produced rows");
    for r in rows {
        assert!(
            r["when_ts"].is_null(),
            "a project with no created_at/updated_at must leave when_ts null, \
             never a fabricated epoch — got {} on kind={}",
            r["when_ts"],
            r["kind"]
        );
    }
}
