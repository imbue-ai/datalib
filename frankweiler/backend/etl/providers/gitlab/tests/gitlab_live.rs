//! Live GitLab single-MR download + translate test.
//!
//! Hits real `gitlab.com/api/v4` via `latchkey curl`, downloads ONE MR
//! into a hermetic tempdir, translates it, and insta-snapshots a
//! stable view.
//!
//! Default target is generally_intelligent MR !7643. Override with
//! `GITLAB_TEST_MR=<namespace/project!IID-or-URL>`.
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo. Run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim
//! cargo test -p frankweiler-etl-gitlab --test gitlab_live -- --ignored
//! ```

use frankweiler_etl_gitlab::extract::{self as gitlab, parse_mr_ref, FetchOptions};
use frankweiler_etl_gitlab::translate::{parse_api_dir, render_gitlab};
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

    let opts = FetchOptions {
        out_dir: tmp.clone(),
        single_mr: Some((proj.clone(), iid)),
        ..Default::default()
    };
    gitlab::fetch(opts).await.expect("gitlab fetch failed");

    let parsed = parse_api_dir(&tmp).expect("parse_api_dir");
    assert_eq!(parsed.merge_requests.len(), 1, "expected exactly one MR");
    let mr = &parsed.merge_requests[0];

    let render_root = tmp.clone();
    render_gitlab(&parsed, &render_root).expect("render_gitlab failed");

    let qmd_rel = frankweiler_etl_gitlab::translate::render::mr_qmd_path_rel(
        &mr.project_full_path,
        mr.mr_iid,
    );
    let qmd_abs = render_root.join(&qmd_rel);
    assert!(
        qmd_abs.exists(),
        "rendered md missing: {}",
        qmd_abs.display()
    );
    let sidecar = qmd_abs.with_extension("grid_rows.json");
    assert!(sidecar.exists(), "sidecar missing: {}", sidecar.display());

    let mut sections: Vec<&'static str> = Vec::new();
    use frankweiler_etl_gitlab::translate::parse::NoteSection;
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
        "rendered_md_exists": qmd_abs.exists(),
        "sidecar_exists": sidecar.exists(),
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!("gitlab_live_single_mr_snapshot", view);
    });
}
