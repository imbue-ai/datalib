//! Integration coverage for the Claude Projects mirror.
//!
//! The insta golden (`claude_render`) renders projects out of the
//! **legacy JSON tree**, which is not the production path. This test
//! covers the production one end to end: synthesize playback fixtures →
//! `download::fetch` → doltlite raw store → `render::parse::parse`.
//!
//! It also pins the two incrementality claims that are easy to break
//! silently, because breaking them costs correctness nothing and only
//! shows up as extra requests:
//!
//!   * a second run with unchanged upstream re-fetches neither the
//!     project metadata nor its knowledge documents;
//!   * a changed project `updated_at` forces both.
//!
//! Both scenarios live under **one** `#[tokio::test]` on purpose: the
//! playback root is selected by a process-wide env var, and the test
//! harness runs `#[test]` fns on parallel threads, so two of them each
//! pointing `DATALIB_HTTP_PLAYBACK` at their own tempdir race and one
//! reads the other's fixtures. Same reason `reset_and_redownload.rs`
//! holds exactly one test.

use std::fs;
use std::time::Duration;

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_claude::download::{fetch, FetchOptions};
use datalib_etl_claude::render::parse::parse;
use datalib_etl_claude::synthesize::ClaudeSynth;
use serde_json::{json, Value};
use tempfile::tempdir;

const ORG: &str = "org-a";
const PROJECT: &str = "proj-1";

fn conversations() -> Value {
    json!([{
        "uuid": "c1",
        "name": "First",
        "updated_at": "2025-01-02T00:00:00Z",
        "organization_uuid": ORG,
        "account": {"uuid": "acct-1"},
        "project": {"uuid": PROJECT},
        "chat_messages": [],
        "_source": {"via": "claude.ai/api", "org_uuid": ORG},
    }])
}

const OTHER_PROJECT: &str = "proj-2";

/// A second project in the same org, so `sync.project_uuids` has
/// something to leave out.
fn other_project() -> Value {
    json!({
        "uuid": OTHER_PROJECT,
        "name": "Holodeck Scratch",
        "creator": {"uuid": "acct-1"},
        "created_at": "2025-01-01T00:00:00Z",
        "updated_at": "2025-01-01T00:00:00Z",
        "_source": {"org_uuid": ORG},
        "docs": [{
            "uuid": "doc-3",
            "file_name": "safeties.md",
            "content": "Leave them on.",
            "created_at": "2025-01-01T03:00:00Z",
        }],
    })
}

fn project(updated_at: &str) -> Value {
    json!({
        "uuid": PROJECT,
        "name": "Bridge Operations",
        "description": "Standing bridge-watch context.",
        "prompt_template": "Answer as an operations officer.",
        "creator": {"uuid": "acct-1"},
        "created_at": "2025-01-01T00:00:00Z",
        "updated_at": updated_at,
        "_source": {"org_uuid": ORG, "org_name": "USS Enterprise"},
        "docs": [
            {
                "uuid": "doc-2",
                "file_name": "escalation.txt",
                "content": "Ops -> Tactical -> XO -> Captain.",
                "created_at": "2025-01-01T02:00:00Z",
            },
            {
                "uuid": "doc-1",
                "file_name": "hailing.md",
                "content": "# Hailing\n\nSubspace band 3.",
                "created_at": "2025-01-01T01:00:00Z",
            },
        ],
    })
}

/// Write the snapshot tree the synthesizer reads and (re)generate the
/// playback fixtures from it.
fn seed(api: &std::path::Path, playback: &std::path::Path, project_updated_at: &str) {
    fs::create_dir_all(api.join("projects")).unwrap();
    fs::write(
        api.join("conversations.json"),
        serde_json::to_vec_pretty(&conversations()).unwrap(),
    )
    .unwrap();
    fs::write(
        api.join("users.json"),
        serde_json::to_vec_pretty(&json!([{"uuid": "acct-1"}])).unwrap(),
    )
    .unwrap();
    fs::write(
        api.join("projects").join("p1.json"),
        serde_json::to_vec_pretty(&project(project_updated_at)).unwrap(),
    )
    .unwrap();
    fs::write(
        api.join("projects").join("p2.json"),
        serde_json::to_vec_pretty(&other_project()).unwrap(),
    )
    .unwrap();
    ClaudeSynth::new(api).synthesize(playback).unwrap();
}

fn opts(raw: &std::path::Path, api: &std::path::Path) -> FetchOptions {
    FetchOptions {
        db_path: raw.to_path_buf(),
        export_dir: Some(api.to_path_buf()),
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: Vec::new(),
        projects: true,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_mirror_end_to_end() {
    round_trip_and_only_refetch_when_upstream_moves().await;
    project_uuids_bounds_the_walk().await;
    projects_can_be_disabled().await;
    conv_uuids_scopes_conversations_not_projects().await;
}

/// `conv_uuids` scopes *conversations*. It used to short-circuit the
/// whole run before the project walk, so a targeted refetch mirrored no
/// projects at all — and `sync.projects` (default on) and an explicit
/// `sync.project_uuids` were both silently ignored.
///
/// That is the wrong default precisely because of what the targeted
/// conversation needs: it resolves its `project` grid column through
/// `project_name_by_uuid`, so with no projects mirrored the column shows
/// a bare UUID instead of the project's name.
async fn conv_uuids_scopes_conversations_not_projects() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback, "2025-01-03T00:00:00Z");
    std::env::set_var(PLAYBACK_ENV, &playback);

    let s = fetch(FetchOptions {
        conv_uuids: vec!["c1".to_string()],
        ..opts(&raw, &api)
    })
    .await
    .unwrap();
    assert_eq!(
        s.projects_fetched, 2,
        "a targeted chat refetch still mirrors projects"
    );
    assert_eq!(s.project_docs_fetched, 3, "and their knowledge docs");

    let parsed = parse(&raw, None).expect("parse the raw store");
    assert_eq!(
        parsed.project_name_by_uuid.get(PROJECT).map(String::as_str),
        Some("Bridge Operations"),
        "so the targeted conversation resolves a name, not a bare UUID"
    );

    // The two knobs are independent: `project_uuids` narrows the project
    // walk whether or not conversations are scoped.
    let raw2 = d.path().join("raw2");
    fs::create_dir_all(&raw2).unwrap();
    let s2 = fetch(FetchOptions {
        conv_uuids: vec!["c1".to_string()],
        project_uuids: vec![PROJECT.to_string()],
        ..opts(&raw2, &api)
    })
    .await
    .unwrap();
    assert_eq!(
        s2.projects_fetched, 1,
        "an explicit project_uuids is honored, not swallowed by conv_uuids"
    );

    // And `projects = false` still switches it off in this mode.
    let raw3 = d.path().join("raw3");
    fs::create_dir_all(&raw3).unwrap();
    let s3 = fetch(FetchOptions {
        conv_uuids: vec!["c1".to_string()],
        projects: false,
        ..opts(&raw3, &api)
    })
    .await
    .unwrap();
    assert_eq!(s3.projects_fetched, 0, "the off switch still works");
}

async fn round_trip_and_only_refetch_when_upstream_moves() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback, "2025-01-03T00:00:00Z");
    std::env::set_var(PLAYBACK_ENV, &playback);

    // ── Run 1: cold ───────────────────────────────────────────────
    let s1 = fetch(opts(&raw, &api)).await.unwrap();
    assert_eq!(s1.projects_fetched, 2, "cold run must store both projects");
    assert_eq!(
        s1.project_docs_fetched, 3,
        "cold run must store every project's docs"
    );
    assert_eq!(s1.projects_skipped, 0);
    assert_eq!(s1.project_docs_skipped, 0);

    // ── The doltlite → render read path ───────────────────────────
    let parsed = parse(&raw, None).expect("parse the raw store");
    assert_eq!(parsed.projects.len(), 2, "both projects should render");
    let p = parsed
        .projects
        .iter()
        .find(|p| p.project_uuid == PROJECT)
        .expect("the seeded project");
    assert_eq!(p.name.as_deref(), Some("Bridge Operations"));
    assert_eq!(
        p.description.as_deref(),
        Some("Standing bridge-watch context.")
    );
    assert_eq!(
        p.prompt_template.as_deref(),
        Some("Answer as an operations officer."),
        "custom instructions have to survive the round trip; they are \
         the one project field with no other home"
    );
    assert_eq!(p.org_uuid.as_deref(), Some(ORG));
    assert_eq!(p.account_uuid, "acct-1");

    // Docs come back sorted by (created_at, uuid), not upstream order —
    // the fixture deliberately lists doc-2 first.
    let names: Vec<&str> = p
        .docs
        .iter()
        .filter_map(|d| d.file_name.as_deref())
        .collect();
    assert_eq!(names, vec!["hailing.md", "escalation.txt"]);
    assert_eq!(
        p.docs[0].content.as_deref(),
        Some("# Hailing\n\nSubspace band 3."),
        "knowledge-doc text rides inline and must reach render verbatim"
    );

    // The name index covers every stored project so a conversation can
    // resolve its `project` grid column.
    assert_eq!(
        parsed.project_name_by_uuid.get(PROJECT).map(String::as_str),
        Some("Bridge Operations")
    );

    // ── Run 2: nothing moved upstream ─────────────────────────────
    let s2 = fetch(opts(&raw, &api)).await.unwrap();
    assert_eq!(
        s2.projects_fetched, 0,
        "unchanged project metadata must not be rewritten"
    );
    assert_eq!(s2.projects_skipped, 2);
    assert_eq!(
        s2.project_docs_fetched, 0,
        "a fresh docs sweep marker must suppress the docs request"
    );
    assert_eq!(s2.project_docs_skipped, 2);

    // ── Run 3: upstream bumped `updated_at` ───────────────────────
    seed(&api, &playback, "2025-06-01T00:00:00Z");
    let s3 = fetch(opts(&raw, &api)).await.unwrap();
    assert_eq!(
        s3.projects_fetched, 1,
        "only the project whose updated_at moved should be re-stored"
    );
    assert_eq!(
        s3.project_docs_fetched, 2,
        "changed project metadata must also re-pull its docs, since we \
         can't rely on updated_at moving for a doc-only edit"
    );
    assert_eq!(
        s3.projects_skipped, 1,
        "the untouched project must still be skipped"
    );
}

/// `sync.project_uuids` bounds the walk to the named projects. The
/// per-org listing still happens (it is one request and the source of
/// the metadata), but nothing outside the set is stored.
async fn project_uuids_bounds_the_walk() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback, "2025-01-03T00:00:00Z");
    std::env::set_var(PLAYBACK_ENV, &playback);

    let s = fetch(FetchOptions {
        // Given as a paste-able URL to pin that the same
        // `normalize_id_token` treatment `conv_uuids` gets applies here.
        project_uuids: vec![format!("https://claude.ai/project/{PROJECT}")],
        ..opts(&raw, &api)
    })
    .await
    .unwrap();
    assert_eq!(s.projects_fetched, 1, "only the named project is stored");
    assert_eq!(s.project_docs_fetched, 2, "and only its docs");

    let parsed = parse(&raw, None).expect("parse the raw store");
    let uuids: Vec<&str> = parsed
        .projects
        .iter()
        .map(|p| p.project_uuid.as_str())
        .collect();
    assert_eq!(
        uuids,
        vec![PROJECT],
        "the excluded project must be absent from the store entirely"
    );
}

/// `sync.projects = false` is a real off switch, not a no-op: no
/// project row is written and no project request is made.
async fn projects_can_be_disabled() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback, "2025-01-03T00:00:00Z");
    std::env::set_var(PLAYBACK_ENV, &playback);

    let s = fetch(FetchOptions {
        projects: false,
        ..opts(&raw, &api)
    })
    .await
    .unwrap();
    assert_eq!(s.projects_fetched, 0);
    assert_eq!(s.project_docs_fetched, 0);
    assert_eq!(s.errors, 0);

    let parsed = parse(&raw, None).expect("parse the raw store");
    assert!(parsed.projects.is_empty(), "no projects should be stored");
    assert!(
        parsed.project_name_by_uuid.is_empty(),
        "and nothing to resolve conversation project names against"
    );
    // The conversation still renders; its `project` column falls back
    // to the bare UUID rather than going blank.
    assert_eq!(parsed.conversations.len(), 1);
    assert_eq!(
        parsed.conversations[0].conv.project_uuid.as_deref(),
        Some(PROJECT)
    );
}
