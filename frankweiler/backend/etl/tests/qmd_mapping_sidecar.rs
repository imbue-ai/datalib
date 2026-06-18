//! Sidecar-driven equivalent of the Python `test_qmd_bridge_integration.py`.
//!
//! The Python test ran the live `qmd` CLI against an ingested fixture and
//! loaded grid rows out of a SQL dump. Now that every Translate step emits
//! a `<doc>.grid_rows.json` sidecar, the same hit↔row mapping invariants
//! can be exercised hermetically:
//!
//!   1. Materialize a small `rendered_md/` tree of sidecars in a tmpdir.
//!   2. Walk it, deserialize each `Sidecar`, project to `GridRowRef`.
//!   3. Feed canned qmd stdout fixtures through `runner::parse_stdout`.
//!   4. Assert the strict mapping invariants on the resulting hits.
//!
//! Covers the spiritual equivalents of the Python tests:
//!   * thread-hit → comment rows (uuid-anchored)
//!   * path-fallback → every row for the doc
//!   * `hits_for_row` reverse mapping
//!   * bidirectional coverage: every indexed path resolves to ≥1 row,
//!     every row's qmd_path matches an indexed path.

use std::fs;
use std::path::{Path, PathBuf};

use frankweiler_core::qmd::mapping::norm_path;
use frankweiler_core::qmd::runner::parse_stdout;
use frankweiler_core::qmd::{GridIndex, GridRowRef, QmdHit};
use frankweiler_index_lib::{Sidecar, SidecarHeader};
use frankweiler_schema::grid_rows::GridRow;

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

fn row(uuid: &str, kind: &str, qmd_path: &str, provider: &str) -> GridRow {
    GridRow {
        uuid: uuid.into(),
        provider: provider.into(),
        kind: kind.into(),
        source_label: provider.into(),
        when_ts: Some("2369-04-14T10:00:00+00:00".into()),
        author: None,
        account: None,
        project: None,
        org_uuid: None,
        org_name: None,
        channel: None,
        conversation_name: None,
        conversation_uuid: uuid.into(),
        message_index: None,
        entire_chat: format!("/chat/{uuid}"),
        text: String::new(),
        slack_link: None,
        qmd_path: Some(qmd_path.into()),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(uuid.into()),
    }
}

fn write_sidecar(root: &Path, qmd_path: &str, rows: Vec<GridRow>) {
    let doc_uuid = rows[0].markdown_uuid.clone().unwrap();
    let sidecar = Sidecar {
        header: SidecarHeader {
            markdown_uuid: doc_uuid,
            source_fingerprint: "deadbeef".into(),
            render_version: 1,
        },
        rows,
        edges: Vec::new(),
    };
    let md = root.join(qmd_path);
    let sidecar_path = md.with_extension("grid_rows.json");
    fs::create_dir_all(sidecar_path.parent().unwrap()).unwrap();
    fs::write(&sidecar_path, serde_json::to_string(&sidecar).unwrap()).unwrap();
}

/// Write a real rendered `.md` at `root/qmd_path` whose section divs carry the
/// given uuids in order (one per message), so the line-based resolver has a
/// file to read. Returns the 1-based line number of each anchor.
fn write_md(root: &Path, qmd_path: &str, section_uuids: &[&str]) -> Vec<usize> {
    let mut body =
        String::from("---\nprovider: test\n---\n\n<h1 class=\"page-title\">Title</h1>\n");
    for uuid in section_uuids {
        body.push_str(&format!(
            "\n<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg\">\n\nbody text\n</div>\n"
        ));
    }
    let md = root.join(qmd_path);
    fs::create_dir_all(md.parent().unwrap()).unwrap();
    fs::write(&md, &body).unwrap();
    // Report each anchor's 1-based line so callers can craft `@@ -N` snippets.
    body.lines()
        .enumerate()
        .filter(|(_, l)| l.contains("data-section-uuid="))
        .map(|(i, _)| i + 1)
        .collect()
}

fn collect_sidecars(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_sidecars(&p, out);
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".grid_rows.json"))
        {
            out.push(p);
        }
    }
}

fn load_grid_rows(root: &Path) -> Vec<GridRowRef> {
    let mut sidecars = Vec::new();
    collect_sidecars(root, &mut sidecars);
    let mut out = Vec::new();
    for path in sidecars {
        let bytes = fs::read(&path).unwrap();
        let sidecar: Sidecar = serde_json::from_slice(&bytes).unwrap();
        for r in sidecar.rows {
            out.push(GridRowRef {
                uuid: r.uuid,
                kind: r.kind,
                qmd_path: r.qmd_path.unwrap_or_default(),
                provider: r.provider,
            });
        }
    }
    out
}

/// Synthesize a two-document fixture tree under `root/rendered_md/`:
///
///   * One anthropic chat doc (Chat row + 2 message rows).
///   * One github PR thread doc (3 PR Comment rows).
fn make_fixture(root: &Path) {
    let chat = "rendered_md/anthropic/acct/llm_chats/c001__klingon_diplomacy.md";
    write_sidecar(
        root,
        chat,
        vec![
            row(
                "c0000001-1701-4d00-8000-00000000c001",
                "Chat",
                chat,
                "anthropic",
            ),
            row(
                "30000001-1701-4d00-8000-000000030001",
                "User Input",
                chat,
                "anthropic",
            ),
            row(
                "30000002-1701-4d00-8000-000000030002",
                "LLM Response",
                chat,
                "anthropic",
            ),
        ],
    );

    let pr_thread = "rendered_md/github/enterprise-d/replicator/pr-42__recalibrate-tea/threads/t01__earl-grey.md";
    write_sidecar(
        root,
        pr_thread,
        vec![
            row(
                "aaaaaaaa-bbbb-cccc-dddd-000000000001",
                "GitHub PR Comment",
                pr_thread,
                "github",
            ),
            row(
                "aaaaaaaa-bbbb-cccc-dddd-000000000002",
                "GitHub Review Comment",
                pr_thread,
                "github",
            ),
            row(
                "aaaaaaaa-bbbb-cccc-dddd-000000000003",
                "GitHub PR Comment",
                pr_thread,
                "github",
            ),
        ],
    );
}

/// Build a qmd stdout fixture that `parse_stdout` will consume.
///
/// The path is wrapped in the same `qmd://mirror/...` URI the real CLI
/// emits, lowercased + `[_-]+` collapsed the way qmd's indexer does.
fn fake_stdout(hits: &[(&str, &str)]) -> String {
    let entries: Vec<serde_json::Value> = hits
        .iter()
        .map(|(path, snippet)| {
            serde_json::json!({
                "file": format!("qmd://mirror/{}", norm_path(path)),
                "score": 0.9,
                "snippet": snippet,
                "docid": "d",
                "title": "t",
            })
        })
        .collect();
    format!(
        "qmd: ready [0/3]\n{}\n",
        serde_json::to_string(&entries).unwrap()
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn sidecar_walk_builds_grid_index() {
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());

    let rows = load_grid_rows(tmp.path());
    assert_eq!(rows.len(), 6);
    let kinds: std::collections::HashSet<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains("Chat"));
    assert!(kinds.contains("GitHub PR Comment"));
}

#[test]
fn line_resolves_to_single_message_row() {
    // A hit whose diff-header line lands in the second message resolves to
    // only that row, even though the chat doc has three rows under one
    // qmd_path — no snippet anchor required.
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());
    let chat = "rendered_md/anthropic/acct/llm_chats/c001__klingon_diplomacy.md";
    let anchors = write_md(
        tmp.path(),
        chat,
        &[
            "30000001-1701-4d00-8000-000000030001",
            "30000002-1701-4d00-8000-000000030002",
        ],
    );
    let idx = GridIndex::new(tmp.path(), load_grid_rows(tmp.path()));

    // `@@ -<second-anchor>,4 @@` → the matched line is on the 2nd message div.
    let snip = format!("@@ -{},4 @@ (0 before, 3 after)\n## Assistant", anchors[1]);
    let stdout = fake_stdout(&[(chat, snip.as_str())]);
    let hits = parse_stdout(&stdout).unwrap();
    let rows = idx.rows_for_hits(&hits);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uuid, "30000002-1701-4d00-8000-000000030002");
    assert_eq!(rows[0].kind, "LLM Response");
}

#[test]
fn path_fallback_returns_all_rows_for_doc() {
    // No `m-{uuid}` in the snippet → fall back to every row whose
    // normalized qmd_path matches the hit's path. The chat doc has 3
    // rows; all should come back.
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());
    // No real `.md` on disk for this doc → the line can't be pinned, so the
    // whole document surfaces.
    let idx = GridIndex::new(tmp.path(), load_grid_rows(tmp.path()));

    let stdout = fake_stdout(&[(
        "rendered_md/anthropic/acct/llm_chats/c001__klingon_diplomacy.md",
        "no anchors here",
    )]);
    let hits = parse_stdout(&stdout).unwrap();
    let rows = idx.rows_for_hits(&hits);
    let uuids: std::collections::HashSet<&str> = rows.iter().map(|r| r.uuid.as_str()).collect();
    assert_eq!(uuids.len(), 3);
    assert!(uuids.contains("c0000001-1701-4d00-8000-00000000c001"));
}

#[test]
fn thread_hit_returns_comment_rows_not_container() {
    // The PR-42 thread doc has 3 comment-level rows and no container
    // "GitHub PR" row (the container lives in a sibling index.md doc
    // that we deliberately don't materialize here). A hit on the
    // thread should resolve to comment rows only — the strict v1
    // semantics the Python integration test asserts.
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());
    let idx = GridIndex::new(tmp.path(), load_grid_rows(tmp.path()));

    let stdout = fake_stdout(&[(
        "rendered_md/github/enterprise-d/replicator/pr-42__recalibrate-tea/threads/t01__earl-grey.md",
        "water temperature drift",
    )]);
    let hits = parse_stdout(&stdout).unwrap();
    let rows = idx.rows_for_hits(&hits);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.kind != "GitHub PR"));
    assert!(rows.iter().all(|r| matches!(
        r.kind.as_str(),
        "GitHub PR Comment" | "GitHub Review Comment"
    )));
}

#[test]
fn hits_for_row_reverse_mapping() {
    // Pick a known comment row; ask which of a set of hits resolve to it.
    // hits_for_row is defined in terms of the forward (line-based) mapping.
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());
    let pr_thread =
        "rendered_md/github/enterprise-d/replicator/pr-42__recalibrate-tea/threads/t01__earl-grey.md";
    let anchors = write_md(
        tmp.path(),
        pr_thread,
        &[
            "aaaaaaaa-bbbb-cccc-dddd-000000000001",
            "aaaaaaaa-bbbb-cccc-dddd-000000000002",
            "aaaaaaaa-bbbb-cccc-dddd-000000000003",
        ],
    );
    let rows_vec = load_grid_rows(tmp.path());
    let idx = GridIndex::new(tmp.path(), rows_vec.clone());

    let target = rows_vec
        .iter()
        .find(|r| r.uuid == "aaaaaaaa-bbbb-cccc-dddd-000000000002")
        .cloned()
        .unwrap();

    let s_target = format!("@@ -{},4 @@ (0 before, 1 after)\nx", anchors[1]);
    let s_other = format!("@@ -{},4 @@ (0 before, 1 after)\nx", anchors[0]);
    let s_diff = format!("@@ -{},4 @@ (0 before, 1 after)\nx", anchors[1]);
    let stdout = fake_stdout(&[
        // Same doc, line lands in the target (2nd) message → matches.
        (pr_thread, s_target.as_str()),
        // Same doc, line lands in a *different* message → must NOT match.
        (pr_thread, s_other.as_str()),
        // Same doc, no diff header → whole-doc fallback includes the target.
        (pr_thread, "nothing anchored"),
        // Different doc altogether → must NOT match.
        (
            "rendered_md/anthropic/acct/llm_chats/c001__klingon_diplomacy.md",
            s_diff.as_str(),
        ),
    ]);
    let hits = parse_stdout(&stdout).unwrap();
    let back = idx.hits_for_row(&target, &hits);
    assert_eq!(back.len(), 2, "target-line hit + whole-doc fallback");
}

#[test]
fn bidirectional_coverage_every_indexed_path_resolves() {
    // Spiritual equivalent of test_every_indexed_doc_maps_to_a_grid_row
    // + test_every_grid_row_has_an_indexed_doc. The "index" here is the
    // set of qmd_paths we'd expect qmd to surface — i.e. one per
    // sidecar — and every grid row's qmd_path must match one of them.
    let tmp = tempfile::tempdir().unwrap();
    make_fixture(tmp.path());
    let rows_vec = load_grid_rows(tmp.path());
    let idx = GridIndex::new(tmp.path(), rows_vec.clone());

    // Collect the on-disk paths of the rendered docs (one per sidecar).
    let mut sidecars = Vec::new();
    collect_sidecars(tmp.path(), &mut sidecars);
    let indexed_paths: Vec<String> = sidecars
        .iter()
        .map(|p| {
            let rel = p.strip_prefix(tmp.path()).unwrap();
            rel.to_string_lossy().replace(".grid_rows.json", ".md")
        })
        .collect();

    // (1) Every indexed path resolves to at least one row via the
    //     path-fallback (empty snippet).
    for p in &indexed_paths {
        let hit = QmdHit {
            path: norm_path(p),
            score: 0.0,
            snippet: String::new(),
            docid: String::new(),
            title: String::new(),
        };
        let rows = idx.rows_for_hit(&hit);
        assert!(!rows.is_empty(), "indexed path with no rows: {p}");
    }

    // (2) Every grid row's qmd_path matches one of the indexed paths
    //     (after normalization).
    let norm_indexed: std::collections::HashSet<String> =
        indexed_paths.iter().map(|p| norm_path(p)).collect();
    for r in &rows_vec {
        assert!(
            norm_indexed.contains(&norm_path(&r.qmd_path)),
            "row {} has no indexed doc ({})",
            r.uuid,
            r.qmd_path
        );
    }
}
