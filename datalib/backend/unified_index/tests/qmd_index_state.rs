//! `QmdIndexReader` against the real qmd index the TNG fixture builds.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::qmd::index_state::{file_sha256_hex, resolve_markdown_states};
use datalib_unified_index::qmd::QmdIndexReader;
use datalib_unified_index::query::parse_query;
use datalib_unified_index::repo::IndexRepo;

/// Resolve a fixture, runfiles first (bazel test) then the `bazel-bin`
/// convenience symlink (plain `cargo test`) — same two-path resolution
/// as `fixture_db_snapshot.rs`, and the same loud panic rather than a
/// silent skip.
fn fixture(rel: &str) -> PathBuf {
    if let Ok(r) = runfiles::Runfiles::create() {
        if let Some(c) = r.rlocation(format!("_main/tests/fixtures/{rel}")) {
            if c.exists() {
                return c;
            }
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    cargo_root
        .join(format!("../../../bazel-bin/tests/fixtures/{rel}"))
        .canonicalize()
        .unwrap_or_else(|_| {
            panic!(
                "fixture {rel} not found. Run `bazelisk build \
                 //tests/fixtures:ingested_tng //tests/fixtures:ingested_tng_qmd` first."
            )
        })
}

fn materialize_root(dst: &Path) {
    for tar in ["ingested/qmd.tar", "ingested/qmd-index.tar"] {
        let status = Command::new("tar")
            .arg("-xf")
            .arg(fixture(tar))
            .arg("-C")
            .arg(dst)
            .arg("--strip-components=1")
            .status()
            .expect("spawn tar");
        assert!(status.success(), "extracting {tar} failed: {status}");
    }
}

fn materialize_root_with_grid(dst: &Path) {
    materialize_root(dst);
    let grid_dir = datalib_core::layout::grid_index_dir(dst);
    std::fs::create_dir_all(&grid_dir).expect("create grid dir");
    let db = grid_dir.join("db.doltlite_db");
    std::fs::copy(fixture("ingested/backend_index.doltlite_db"), &db).expect("copy grid index");
    // The fixture output is read-only in the runfiles tree; doltlite
    // wants to open it writable even though we only read.
    let mut perms = std::fs::metadata(&db).expect("stat").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&db, perms).expect("chmod");
}

/// The groups the fixture embeds, from `tests/fixtures/qmd_groups.bzl`
/// by way of the BUILD file's `env`. Every other group is indexed and
/// not embedded, deliberately: it is what keeps the `Embedded` column
/// from being the `Indexed` column twice.
fn embedded_groups() -> BTreeSet<String> {
    let raw = std::env::var("QMD_FIXTURE_EMBEDDED_GROUPS")
        .expect("QMD_FIXTURE_EMBEDDED_GROUPS unset — the BUILD rule sets it from qmd_groups.bzl");
    let groups: BTreeSet<String> = raw.split(',').map(str::to_string).collect();
    assert!(!groups.is_empty(), "the fixture embeds at least one group");
    groups
}

/// The group a rendered file belongs to: the first segment of its path
/// under the data root, which is the collection qmd filed it under.
fn group_of(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .expect("rendered file is under the root")
        .components()
        .next()
        .expect("a group segment")
        .as_os_str()
        .to_string_lossy()
        .into_owned()
}

fn rendered_markdowns(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "md")
                && p.components().any(|c| c.as_os_str() == "render_markdown")
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn every_rendered_document_is_reported_indexed_and_embedded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root(root);

    let files = rendered_markdowns(root);
    assert!(
        files.len() > 20,
        "fixture should carry a few dozen rendered docs, found {}",
        files.len()
    );

    let reader = QmdIndexReader::open(root)
        .await
        .expect("open the fixture qmd index")
        .expect("the fixture ships an index.sqlite");

    let hashes: Vec<String> = files
        .iter()
        .map(|p| file_sha256_hex(p).expect("hash a rendered file"))
        .collect();
    let states = reader
        .states_for_hashes(&hashes)
        .await
        .expect("query the qmd index");

    // The load-bearing assertion: our digest of the file on disk is the
    // key qmd filed it under.
    let missing: Vec<&PathBuf> = files
        .iter()
        .zip(&hashes)
        .filter(|(_, h)| !states.contains_key(*h))
        .map(|(p, _)| p)
        .collect();
    assert!(
        missing.is_empty(),
        "these rendered documents did not match any qmd `documents.hash` — \
         qmd's content hashing has probably changed: {missing:?}"
    );

    // The fixture embeds some groups and not others, so the `embedded`
    // half of the answer has to follow the group line exactly: a
    // complete vector set inside an embedded group, none outside. This
    // is what separates the two grid columns from being one column
    // twice.
    let embedded = embedded_groups();
    let wrong: Vec<(&PathBuf, bool)> = files
        .iter()
        .zip(&hashes)
        .map(|(p, h)| (p, states[h].embedded))
        .filter(|(p, is_embedded)| embedded.contains(&group_of(root, p)) != *is_embedded)
        .collect();
    assert!(
        wrong.is_empty(),
        "embedded state disagrees with the fixture's embedded groups {embedded:?}: {wrong:?}"
    );
    let expected_embedded = files
        .iter()
        .filter(|p| embedded.contains(&group_of(root, p)))
        .count();
    assert!(
        expected_embedded > 0 && expected_embedded < files.len(),
        "the fixture should embed some documents and leave others: {expected_embedded} of {}",
        files.len()
    );

    let summary = reader.summary().await.expect("summary");
    assert_eq!(
        summary.documents as usize,
        files.len(),
        "collection document count should match the rendered tree"
    );
    assert_eq!(
        summary.embedded as usize, expected_embedded,
        "the summary counts exactly the embedded groups' documents"
    );
}

/// A file the index has never seen reports un-indexed rather than
/// erroring — the ordinary state for a document rendered since the last
/// sync.
#[tokio::test]
async fn an_unknown_hash_is_simply_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root(root);

    let reader = QmdIndexReader::open(root)
        .await
        .expect("open")
        .expect("index present");
    let states = reader
        .states_for_hashes(&["0".repeat(64)])
        .await
        .expect("query");
    assert!(states.is_empty(), "unknown hash should map to nothing");
}

/// A data root with no `index.sqlite` — every root before its first
/// sync — opens to `None` rather than failing.
#[tokio::test]
async fn a_root_without_an_index_opens_to_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let reader = QmdIndexReader::open(tmp.path()).await.expect("open");
    assert!(reader.is_none());
}

/// Editing a rendered document must flip **exactly its own rows** to
/// not-indexed, and restoring it must flip them back — with every other
/// document's rows untouched throughout.
#[tokio::test]
async fn editing_one_document_flips_only_its_own_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);

    let repo = DoltRepo::open(Arc::new(root.to_path_buf()))
        .await
        .expect("open the fixture grid index");
    let reader = QmdIndexReader::open(root)
        .await
        .expect("open the fixture qmd index")
        .expect("the fixture ships an index.sqlite");

    // Group the grid's rows by the document they live in.
    let rows = repo
        .search(&parse_query(""), 10_000)
        .await
        .expect("search the grid index");
    assert!(
        rows.len() > 50,
        "fixture should be row-rich, got {}",
        rows.len()
    );
    let mut by_doc: std::collections::HashMap<String, Vec<String>> = Default::default();
    for r in &rows {
        if let Some(md) = &r.markdown_uuid {
            by_doc.entry(md.clone()).or_default().push(r.uuid.clone());
        }
    }
    let all_uuids: Vec<String> = by_doc.keys().cloned().collect();
    assert!(all_uuids.len() > 10, "expected many documents");

    // Baseline: everything indexed, and embedded exactly where the
    // fixture embeds.
    let embedded = embedded_groups();
    let paths = repo.md_paths_for(&all_uuids).await.expect("md_paths_for");
    let before = resolve_markdown_states(&repo, &reader, &all_uuids)
        .await
        .expect("resolve");
    let unhealthy: Vec<&String> = before
        .iter()
        .filter(|(md, v)| {
            let should_embed = embedded.contains(&group_of(root, &paths[*md]));
            v.indexed != Some(true) || v.embedded != Some(should_embed)
        })
        .map(|(k, _)| k)
        .collect();
    assert!(
        unhealthy.is_empty(),
        "fixture should start green along the embedded-group line: {unhealthy:?}"
    );

    // Pick an embedded document that several grid rows share, so "only
    // its rows" is a meaningful claim rather than a single-row
    // coincidence, and so both columns have somewhere to fall from.
    let (target, target_rows) = by_doc
        .iter()
        .filter(|(md, rs)| rs.len() >= 3 && embedded.contains(&group_of(root, &paths[*md])))
        .min_by_key(|(md, _)| (*md).clone())
        .map(|(md, rs)| (md.clone(), rs.clone()))
        .expect("an embedded document with at least 3 grid rows");

    // Edit the target's file — exactly what a re-render does to a
    // document the indexer has not caught up with.
    let path = paths[&target].clone();
    let original = std::fs::read(&path).expect("read target");
    let mut edited = original.clone();
    edited.extend_from_slice(b"\n<!-- re-rendered since the last index run -->\n");
    std::fs::write(&path, &edited).expect("write target");

    let during = resolve_markdown_states(&repo, &reader, &all_uuids)
        .await
        .expect("resolve");
    let flipped: BTreeSet<&String> = during
        .iter()
        .filter(|(_, v)| v.indexed != Some(true))
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        flipped,
        BTreeSet::from([&target]),
        "editing one document should flip exactly that document"
    );
    // The edited document is out of the index entirely — not merely
    // un-embedded. Both columns move together, because the hash it was
    // filed under no longer describes any file.
    assert_eq!(during[&target].indexed, Some(false));
    assert_eq!(during[&target].embedded, Some(false));
    // And the rows that share the document are the ones the UI paints:
    // more than one, all pointing at the same state.
    assert!(
        target_rows.len() >= 3,
        "target should back several grid rows, got {}",
        target_rows.len()
    );

    // Restore: the state is derived live, so it must come straight
    // back. (A cache keyed on uuid or path would pass every assertion
    // above and fail this one.)
    std::fs::write(&path, &original).expect("restore target");
    let after = resolve_markdown_states(&repo, &reader, &all_uuids)
        .await
        .expect("resolve");
    assert_eq!(
        after, before,
        "restoring the file must restore the reported state exactly"
    );
}

/// A row whose document has no `markdowns` row reports unknown — not a
/// red ❌ — and says why.
#[tokio::test]
async fn a_markdown_we_have_no_file_for_is_unknown_not_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);

    let repo = DoltRepo::open(Arc::new(root.to_path_buf()))
        .await
        .expect("open grid index");
    let reader = QmdIndexReader::open(root)
        .await
        .expect("open")
        .expect("index present");

    let got = resolve_markdown_states(&repo, &reader, &["no-such-markdown".to_string()])
        .await
        .expect("resolve");
    let r = &got["no-such-markdown"];
    assert_eq!(r.indexed, None, "unknown, not false");
    assert_eq!(r.embedded, None);
    assert_eq!(r.note.as_deref(), Some("no rendered document"));
}
