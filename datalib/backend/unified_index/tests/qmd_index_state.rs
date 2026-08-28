//! `QmdIndexReader` against the real qmd index the TNG fixture builds.
//!
//! The claim this test exists to defend is the one that would rot
//! silently: **`documents.hash` is the SHA-256 of the rendered file's
//! bytes**, which is the entire basis for joining grid rows to qmd
//! documents without reimplementing qmd's `handelize` path mangling.
//! If a qmd version bump changed the digest, or started hashing
//! normalized rather than raw text, the grid's Indexed / Embedded
//! columns would go all-❌ with nothing else failing. So this test
//! walks the fixture's real rendered markdown tree, hashes every file
//! itself, and asserts the reader reports every one of them indexed.
//!
//! Fixtures: `//tests/fixtures:ingested_tng` (the markdown tree) and
//! `//tests/fixtures:ingested_tng_qmd` (a real qmd index over it,
//! built by the real indexer). Both are already inputs to
//! `//datalib/ui:e2e_test` via `materialize_tng_root`, so depending on
//! them here adds nothing new to a `bazel test //...`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::qmd::index_state::{file_content_hash, resolve_markdown_states};
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

/// Lay both fixture tars down into one directory. They share a `qmd/`
/// staging prefix precisely so they layer: markdown tree plus
/// `unified_index/qmd/index.sqlite` is a complete data root.
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

/// …plus the grid index, at the path `DoltRepo::open` resolves. Same
/// placement `tests/fixtures/materialize_tng_root.sh` uses, so this
/// test and the e2e harness build the same root.
fn materialize_root_with_grid(dst: &Path) {
    materialize_root(dst);
    let grid_dir = dst.join("unified_index").join("grid");
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

/// Every `.md` under a `rendered_md/` directory in the root — the same
/// set the indexer's `*/rendered_md/**/*.md` mask selects.
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
                && p.components().any(|c| c.as_os_str() == "rendered_md")
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
        .map(|p| file_content_hash(p).expect("hash a rendered file"))
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

    // The fixture's indexer runs `embed`, so every document should also
    // carry a complete vector set. This is what separates the two grid
    // columns from being one column twice.
    let unembedded: Vec<&PathBuf> = files
        .iter()
        .zip(&hashes)
        .filter(|(_, h)| !states.get(*h).map(|s| s.embedded).unwrap_or(false))
        .map(|(p, _)| p)
        .collect();
    assert!(
        unembedded.is_empty(),
        "fixture documents indexed but not embedded: {unembedded:?}"
    );

    let summary = reader.summary().await.expect("summary");
    assert_eq!(
        summary.documents as usize,
        files.len(),
        "collection document count should match the rendered tree"
    );
    assert_eq!(
        summary.embedded, summary.documents,
        "the fixture embeds everything it indexes"
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
///
/// This is the behavior the grid's two columns promise, and it is the
/// one that is easy to get subtly wrong: the grid is message-level
/// while the index is document-level, so a bug in either hop (row →
/// document by `markdown_uuid`, or document → qmd by content hash)
/// shows up as neighbouring rows flipping together, or as nothing
/// flipping at all. Both failure modes look plausible on screen.
///
/// Drives `resolve_markdown_states` — the same function the
/// `/qmd_state` handler calls — against the real fixture index, so it
/// cannot pass while the shipped path is broken.
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

    // Pick a document that several grid rows share, so "only its rows"
    // is a meaningful claim rather than a single-row coincidence.
    let (target, target_rows) = by_doc
        .iter()
        .filter(|(_, rs)| rs.len() >= 3)
        .min_by_key(|(md, _)| (*md).clone())
        .map(|(md, rs)| (md.clone(), rs.clone()))
        .expect("a document with at least 3 grid rows");

    // Baseline: everything indexed and embedded.
    let before = resolve_markdown_states(&repo, &reader, &all_uuids)
        .await
        .expect("resolve");
    let unhealthy: Vec<&String> = before
        .iter()
        .filter(|(_, v)| v.indexed != Some(true) || v.embedded != Some(true))
        .map(|(k, _)| k)
        .collect();
    assert!(
        unhealthy.is_empty(),
        "fixture should start all-green: {unhealthy:?}"
    );

    // Edit the target's file — exactly what a re-render does to a
    // document the indexer has not caught up with.
    let path = repo
        .md_paths_for(std::slice::from_ref(&target))
        .await
        .expect("md_paths_for")
        .remove(&target)
        .expect("target has a rendered file");
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
