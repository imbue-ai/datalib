//! Hermetic smoke test for `download::fetch`.
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

use frankweiler_etl::control::DownloadControl;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_fsindex::download::{self, FetchOptions, RawDb};
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
        // blake3 is a 32-byte BLOB; render as hex for the snapshot.
        let blake3_bytes: Vec<u8> = r.try_get("blake3").unwrap();
        let blake3: String = blake3_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let symlink_target: Option<String> = r.try_get("symlink_target").unwrap();
        let identity_uuid: Option<String> = r.try_get("identity_uuid").unwrap();
        out.push_str(&format!(
            "id={id:32} kind={kind:7} size={size:5} blake3={blake3} symlink={symlink_target:?} uuid={identity_uuid:?}\n"
        ));
    }
    out
}

/// Verify the Unison cursor was stored correctly: every FILE row's
/// `file_stats` should carry `stamp_kind = 'inode'` on unix with
/// non-NULL `inode` + `dev`. If this fails, the rescan compare in
/// `stamp::decide` will never reuse anything regardless of how
/// unchanged the file is.
///
/// Dirs and symlinks deliberately get `stamp_kind = 'nostamp'`
/// because they always rehash on every scan (no fast path), so
/// there's no cache to consult for them.
async fn assert_inode_stamp_kind(db_path: &Path) {
    let db = RawDb::open(db_path).await.unwrap();
    let rows = sqlx::query(
        "SELECT file_stats.id, file_stats.stamp_kind, file_stats.inode, file_stats.dev \
         FROM file_stats JOIN files ON files.id = file_stats.id \
         WHERE files.kind = 'file'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(!rows.is_empty(), "no file-kind rows in file_stats");
    for r in rows {
        let id: String = r.try_get("id").unwrap();
        let stamp_kind: String = r.try_get("stamp_kind").unwrap();
        let inode: Option<i64> = r.try_get("inode").unwrap();
        let dev: Option<i64> = r.try_get("dev").unwrap();
        #[cfg(unix)]
        {
            assert_eq!(
                stamp_kind, "inode",
                "file row id={id:?} stamp_kind={stamp_kind} — expected 'inode' on unix"
            );
            assert!(
                inode.is_some(),
                "row id={id:?} has stamp_kind=inode but inode is NULL"
            );
            assert!(
                dev.is_some(),
                "row id={id:?} has stamp_kind=inode but dev is NULL"
            );
        }
        #[cfg(not(unix))]
        {
            let _ = (inode, dev);
            assert_eq!(stamp_kind, "nostamp", "row id={id:?} on non-unix");
        }
    }
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
        control: DownloadControl::default(),
    }
}

/// Read a directory row's `identity_uuid` from the `files` table.
async fn dir_identity_uuid(db_path: &Path, id: &str) -> Option<String> {
    let db = RawDb::open(db_path).await.unwrap();
    let row = sqlx::query("SELECT identity_uuid FROM files WHERE id = ? AND kind = 'dir'")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    row.try_get::<Option<String>, _>("identity_uuid").unwrap()
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
    let summary_a = download::fetch(fetch_opts(&db_path, &root))
        .await
        .expect("initial fetch");
    assert_eq!(summary_a.errors, 0, "no walker errors");
    assert_eq!(summary_a.files_reused, 0, "nothing cached yet");
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
    assert_inode_stamp_kind(&db_path).await;

    insta::assert_snapshot!("initial_scan", dump_files(&db_path).await);

    // ── Phase A2: rescan with no changes — Unison fast path ─────────
    // This is the test that the inode-based cursor is actually
    // doing its job. All four FILE rows should reuse their cached
    // blake3 against the unchanged (mtime, size, inode) triple;
    // only the symlink and the two directories should rehash.
    let summary_a2 = download::fetch(fetch_opts(&db_path, &root))
        .await
        .expect("unchanged rescan");
    assert_eq!(summary_a2.errors, 0);
    #[cfg(unix)]
    {
        assert_eq!(summary_a2.entries_scanned, 7);
        // 4 files (hello, empty, another, nested) reuse from cache.
        assert_eq!(
            summary_a2.files_reused, 4,
            "fast-rescan cache should reuse every unchanged file's blake3; \
             got summary {summary_a2:?}",
        );
        // No file content is re-read on an unchanged rescan; the symlink
        // and the two dirs recompute their hash for free (no bytes).
        assert_eq!(summary_a2.files_hashed, 0, "no file content re-read");
        assert_eq!(summary_a2.dirs, 2);
        assert_eq!(summary_a2.symlinks, 1);
        assert_eq!(
            summary_a2.bytes_hashed, 0,
            "zero bytes hashed when nothing changed"
        );
    }

    // ── Phase B: edits + incremental rescan ─────────────────────────
    // Modify subdir/nested.txt: content change → blake3 change.
    write(&root.join("subdir/nested.txt"), b"nested-modified");
    // Touch hello.txt by re-writing identical bytes: mtime bumps,
    // content unchanged. Rescan should rehash but produce the same
    // blake3.
    write(&root.join("hello.txt"), b"hello world\n");
    // Add a new file under subdir.
    write(&root.join("subdir/added.txt"), b"brand new");
    // DELETE one file. After truncate-and-rebuild, the row should
    // be gone from `files` (visible in the after_edits snapshot).
    fs::remove_file(root.join("empty.txt")).unwrap();

    let summary_b = download::fetch(fetch_opts(&db_path, &root))
        .await
        .expect("incremental fetch");
    assert_eq!(summary_b.errors, 0);
    // Scanned this time: root, hello.txt, hello.link, subdir,
    // subdir/another.txt, subdir/nested.txt, subdir/added.txt = 7.
    // (empty.txt is gone.)
    // Reused: subdir/another.txt is the only file whose
    // (mtime,size,inode) triple is unchanged across the edit set.
    // hello.txt was re-written (mtime bump). nested.txt was
    // re-written (content change). added.txt is new. empty.txt is
    // gone and doesn't appear.
    #[cfg(unix)]
    {
        assert_eq!(summary_b.entries_scanned, 7);
        // another.txt is the only unchanged file → reused.
        assert_eq!(summary_b.files_reused, 1);
        // hello.txt (mtime bump), nested.txt (content), added.txt (new)
        // → 3 files actually re-read and hashed.
        assert_eq!(summary_b.files_hashed, 3);
        assert_eq!(summary_b.dirs, 2);
        assert_eq!(summary_b.symlinks, 1);
    }

    let dump_b = dump_files(&db_path).await;
    // Truncate-and-rebuild: the deleted file must not appear.
    assert!(
        !dump_b.contains("empty.txt"),
        "empty.txt was deleted but row survives: \n{dump_b}",
    );
    insta::assert_snapshot!("after_edits", dump_b);
}

/// Stamping is the same streaming scan plus a post-write enrichment
/// pass: a dir whose cascade enables `stamp_me_with_uuid` gets a UUID
/// breadcrumb written into it and its `files.identity_uuid` set. A
/// second scan is idempotent — it reuses the existing breadcrumb and
/// writes no new ones. This is the path that used to be the untested
/// `legacy_inmemory` branch.
#[tokio::test]
async fn stamping_writes_breadcrumb_and_sets_identity_uuid() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("tree");
    fs::create_dir(&root).unwrap();
    let db_path = tmp.path().join("fsindex.doltlite_db");

    // `subdir` opts into stamping via its own `.fsindex.yaml`; the rest
    // of the tree does not.
    write(&root.join("hello.txt"), b"hello world\n");
    write(
        &root.join("subdir/.fsindex.yaml"),
        b"stamp_me_with_uuid: true\n",
    );
    write(&root.join("subdir/nested.txt"), b"nested");

    let mut opts = fetch_opts(&db_path, &root);
    opts.no_stamp = false;
    let summary = download::fetch(opts).await.expect("stamping fetch");
    assert_eq!(summary.errors, 0);
    assert_eq!(
        summary.stamped_directories, 1,
        "exactly `subdir` should be newly stamped"
    );

    // The breadcrumb file now carries an identity block...
    let breadcrumb = fs::read_to_string(root.join("subdir/.fsindex.yaml")).unwrap();
    assert!(
        breadcrumb.contains("identity:") && breadcrumb.contains("uuid:"),
        "breadcrumb missing identity block:\n{breadcrumb}",
    );
    assert!(
        breadcrumb.contains("stamp_me_with_uuid:"),
        "breadcrumb must preserve the user's stamp_me_with_uuid key:\n{breadcrumb}",
    );

    // ...and the `subdir` row carries the matching identity_uuid, while
    // an un-opted-in dir (root) stays NULL.
    let stamped = dir_identity_uuid(&db_path, "subdir").await;
    assert!(stamped.is_some(), "subdir row should carry identity_uuid");
    assert_eq!(
        dir_identity_uuid(&db_path, "").await,
        None,
        "root opted out, so its identity_uuid stays NULL"
    );

    // Second scan: idempotent. No new breadcrumb, same UUID reused.
    let mut opts2 = fetch_opts(&db_path, &root);
    opts2.no_stamp = false;
    let summary2 = download::fetch(opts2).await.expect("rescan");
    assert_eq!(
        summary2.stamped_directories, 0,
        "rescan reuses the existing breadcrumb — nothing newly stamped"
    );
    assert_eq!(
        dir_identity_uuid(&db_path, "subdir").await,
        stamped,
        "rescan must keep the same identity_uuid"
    );
}
