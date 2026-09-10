// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live GitLab single-MR download + render test.

use datalib_etl_gitlab::ingest::{self as gitlab, parse_mr_ref, FetchOptions};
use datalib_etl_gitlab_render::render::{parse_api_dir, render_gitlab};
use insta::assert_json_snapshot;
use serde_json::json;

const DEFAULT_TARGET_MR: &str = "generally-intelligent/generally_intelligent!7643";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn gitlab_live_single_mr_snapshot() {
    let mr_ref = std::env::var("GITLAB_TEST_MR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TARGET_MR.to_string());
    let (proj, iid) = parse_mr_ref(&mr_ref).expect("parse mr ref");

    let tmp = tempfile::TempDir::with_prefix("gitlab-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading {proj}!{iid} -> {}", tmp.display());

    let db = gitlab::RawDb::open(&gitlab::db_path_for(&tmp))
        .await
        .unwrap();
    let opts = FetchOptions {
        targets: vec![(proj.clone(), iid)],
        ..FetchOptions::new(db.clone())
    };
    let r = gitlab::fetch(opts).await;
    // Seal what `fetch` wrote before anything reads it, the way the
    // download step's `RawStoreSession::finish` does. `fetch` writes rows
    // but commits nothing, and the render-side loader below reads at a
    // pinned commit — so without this it sees an empty store.
    let sealed = datalib_etl::doltlite_raw::commit_run(db.pool(), "gitlab_live: download").await;
    db.close().await;
    r.expect("gitlab fetch failed");
    sealed.expect("seal the raw store");

    let parsed = parse_api_dir(&tmp, None).expect("parse_api_dir");
    assert_eq!(parsed.merge_requests.len(), 1, "expected exactly one MR");
    let mr = &parsed.merge_requests[0];

    let render_root = tmp.clone();
    let stanza = "gitlab";
    let mut docs: Vec<datalib_etl_render::grid_index::RenderedMarkdown> = Vec::new();
    render_gitlab(
        &parsed,
        &render_root,
        stanza,
        &datalib_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
    )
    .expect("render_gitlab failed");

    let qmd_rel = datalib_etl_gitlab_render::render::render::mr_qmd_path_rel(
        stanza,
        &mr.project_full_path,
        mr.mr_iid,
    );
    let qmd_abs = render_root.join(&qmd_rel);
    assert!(
        qmd_abs.exists(),
        "rendered md missing: {}",
        qmd_abs.display()
    );
    // The projection rides on the emitted document now, not in a file
    // beside the markdown.
    assert!(
        docs.iter().any(|d| !d.rows.is_empty()),
        "render emitted no rows"
    );

    let mut sections: Vec<&'static str> = Vec::new();
    use datalib_etl_gitlab_render::render::parse::NoteSection;
    if parsed
        .notes
        .iter()
        .any(|n| n.section == NoteSection::General)
    {
        sections.push("General");
    }
    if parsed
        .notes
        .iter()
        .any(|n| n.section == NoteSection::Inline)
    {
        sections.push("Inline");
    }

    let view = json!({
        "project": mr.project_full_path,
        "mr_iid": mr.mr_iid,
        "has_title": !mr.title.is_empty(),
        "has_web_url": mr.web_url.is_some(),
        "state_known": mr.state.is_some(),
        "note_count": parsed.notes.len(),
        "sections_present": sections,
        "render_markdown_exists": qmd_abs.exists(),
        "rows_emitted": docs.iter().any(|d| !d.rows.is_empty()),
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!("gitlab_live_single_mr_snapshot", view);
    });
}
