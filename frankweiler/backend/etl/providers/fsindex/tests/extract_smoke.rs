//! Hermetic smoke test for `extract::fetch`.
//!
//! Builds a small directory tree in a tempdir, scans it, snapshots
//! the `files` table, then edits the tree (modify, touch, add) and
//! re-scans. Asserts the per-summary cache stats are right (some
//! files reuse, the rest rehash) and snapshots the table again to
//! catch silent regressions in the canonicalization, the symlink
//! handling, or the cascaded `.fsindex.yaml` ignore filter.
//!
//! Update with `cargo insta review` from `frankweiler/backend`.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_fsindex::extract::{self, FetchOptions, RawDb};
use sqlx::Row;
use tempfile::TempDir;

fn write(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn make_initial_tree(root: &Path) {
    write(&root.join(".fsindex.yaml"), b"ignore:\n  - '*.tmp'\n");
    write(&root.join("hello.txt"), b"hello world\n");
    write(&root.join("empty.txt"), b"");
    write(&root.join("subdir/nested.txt"), b"nested");
    write(&root.join("subdir/another.txt"), b"another");
    write(&root.join("subdir/junk.tmp"), b"should not appear");
    #[cfg(unix)]
    symlink("subdir/nested.txt", root.join("hello.link")).unwrap();
}

async fn dump_files(db_path: &Path) -> String {
    let db = RawDb::open(db_path).await.unwrap();
    let rows = sqlx::query(
        "SELECT id, kind, size, blake3, symlink_target, identity_uuid \
         FROM files ORDER BY id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let mut out = String::new();
    for r in rows {
        let id: String = r.try_get("id").unwrap();
        let kind: String = r.try_get("kind").unwrap();
        let size: i64 = r.try_get("size").unwrap();
        let blake3: String = r.try_get("blake3").unwrap();
        let symlink_target: Option<String> = r.try_get("symlink_target").unwrap();
        let identity_uuid: Option<String> = r.try_get("identity_uuid").unwrap();
        out.push_str(&format!(
            "id={id:32} kind={kind:7} size={size:5} blake3={blake3} symlink={symlink_target:?} uuid={identity_uuid:?}\n"
        ));
    }
    out
}

fn fetch_opts(db_path: &Path, root: &Path) -> FetchOptions {
    FetchOptions {
        db_path: db_path.to_path_buf(),
        db: None,
        source_name: "smoke".to_string(),
        root: root.to_path_buf(),
        target_doltlite_branch: None,
        no_stamp: true,
        progress: Progress::noop(),
        control: ExtractControl::default(),
    }
}

#[tokio::test]
async fn initial_scan_and_incremental_rescan() {
    let tmp = TempDir::new().unwrap();
    // Keep the doltlite db OUT of the scan root so the scanner doesn't
    // index its own backing file.
    let root = tmp.path().join("tree");
    fs::create_dir(&root).unwrap();
    let db_path = tmp.path().join("fsindex.doltlite_db");
    make_initial_tree(&root);

    // ── Phase A: initial scan ───────────────────────────────────────
    let summary_a = extract::fetch(fetch_opts(&db_path, &root))
        .await
        .expect("initial fetch");
    assert_eq!(summary_a.errors, 0, "no walker errors");
    assert_eq!(summary_a.entries_reused, 0, "nothing cached yet");
    assert_eq!(
        summary_a.stamped_directories, 0,
        "no_stamp=true, no breadcrumbs written"
    );
    // `junk.tmp` is ignored; `.fsindex.yaml` is scanner metadata, not
    // a content row. So `files` should hold:
    //   root (D), hello.txt (F), empty.txt (F), hello.link (L),
    //   subdir (D), subdir/nested.txt (F), subdir/another.txt (F)
    // = 7 entries on unix; 6 on non-unix (no symlink).
    #[cfg(unix)]
    assert_eq!(summary_a.entries_scanned, 7);
    #[cfg(not(unix))]
    assert_eq!(summary_a.entries_scanned, 6);

    insta::assert_snapshot!("initial_scan", dump_files(&db_path).await);

    // ── Phase B: edits + incremental rescan ─────────────────────────
    // Modify subdir/nested.txt: content change → blake3 change.
    write(&root.join("subdir/nested.txt"), b"nested-modified");
    // Touch hello.txt by re-writing identical bytes: mtime bumps,
    // content unchanged. Rescan should rehash but produce the same
    // blake3.
    write(&root.join("hello.txt"), b"hello world\n");
    // Add a new file under subdir.
    write(&root.join("subdir/added.txt"), b"brand new");

    let summary_b = extract::fetch(fetch_opts(&db_path, &root))
        .await
        .expect("incremental fetch");
    assert_eq!(summary_b.errors, 0);
    // Reused: empty.txt + subdir/another.txt — two file rows whose
    // (mtime, size, inode) triple is unchanged. Everything else
    // rehashes: hello.txt (mtime bump from re-write),
    // subdir/nested.txt (content changed), subdir/added.txt (new),
    // hello.link (symlinks rehash unconditionally),
    // root + subdir (dirs recompute their tree-hash every scan).
    #[cfg(unix)]
    {
        assert_eq!(summary_b.entries_scanned, 8);
        assert_eq!(summary_b.entries_reused, 2);
        assert_eq!(summary_b.entries_rehashed, 6);
    }

    insta::assert_snapshot!("after_edits", dump_files(&db_path).await);

    // CONCERN(deletions-not-reconciled): the walker only emits rows
    // for entries it *sees*. A file that existed at scan-A and is
    // gone at scan-B leaves a stale row in `files` + `file_stats`.
    // For now scans are additive/merge-style. A future reconciliation
    // pass (delete-rows-whose-id-not-in-this-walk) is the obvious
    // fix; not exercised by this test.
}
