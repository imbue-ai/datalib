// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live GitHub single-PR download + render test.
//!
//! Hits real `api.github.com` via `latchkey curl`, downloads ONE PR
//! (meta + comments + reviews) into a hermetic tempdir, renders it,
//! and insta-snapshots a stable view.
//!
//! Default target is the imbue-ai mngr PR #1650 (kept around for this
//! test). Override with `GITHUB_TEST_PR=<owner/repo#NUM-or-URL>`.
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo. Run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-impersonate
//! cargo test -p frankweiler-etl-github --test github_live -- --ignored
//! ```

use frankweiler_etl_github::download::{self as github, parse_pr_ref, FetchOptions};
use frankweiler_etl_github::render::{parse_api_dir, render_github};
use insta::assert_json_snapshot;
use serde_json::json;

const DEFAULT_TARGET_PR: &str = "imbue-ai/mngr#1650";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn github_live_single_pr_snapshot() {
    let pr_ref = std::env::var("GITHUB_TEST_PR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TARGET_PR.to_string());
    let (repo, num) = parse_pr_ref(&pr_ref).expect("parse pr ref");

    let tmp = tempfile::TempDir::with_prefix("github-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] downloading {repo}#{num} -> {}", tmp.display());

    let opts = FetchOptions {
        db_path: tmp.clone(),
        targets: vec![(repo.clone(), num)],
        ..Default::default()
    };
    github::fetch(opts).await.expect("github fetch failed");

    let parsed = parse_api_dir(&tmp).expect("parse_api_dir");
    assert_eq!(parsed.pull_requests.len(), 1, "expected exactly one PR");
    let pr = &parsed.pull_requests[0];

    let render_root = tmp.clone();
    let stanza = "github_live";
    render_github(
        &parsed,
        &render_root,
        stanza,
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &mut |_doc| Ok(()),
    )
    .expect("render_github failed");

    // The rendered doc must exist.
    let qmd_rel = frankweiler_etl_github::render::render::pr_qmd_path_rel(
        stanza,
        &pr.repo_full_name,
        pr.pr_number,
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
    use frankweiler_etl_github::render::parse::CommentSection;
    if parsed
        .comments
        .iter()
        .any(|c| c.section == CommentSection::Review)
    {
        sections.push("Review");
    }
    if parsed
        .comments
        .iter()
        .any(|c| c.section == CommentSection::General)
    {
        sections.push("General");
    }
    if parsed
        .comments
        .iter()
        .any(|c| c.section == CommentSection::Inline)
    {
        sections.push("Inline");
    }

    let view = json!({
        "repo": pr.repo_full_name,
        "pr_number": pr.pr_number,
        "has_title": !pr.title.is_empty(),
        "has_html_url": pr.html_url.is_some(),
        "state_known": pr.state.is_some(),
        "comment_count": parsed.comments.len(),
        "sections_present": sections,
        "rendered_md_exists": qmd_abs.exists(),
        "sidecar_exists": sidecar.exists(),
    });

    insta::with_settings!({ sort_maps => true }, {
        assert_json_snapshot!("github_live_single_pr_snapshot", view);
    });
}
