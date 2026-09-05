//! Hermetic smoke test for `download::fetch`.
//!
//! Builds a small directory tree in a tempdir, scans it, snapshots
//! the `files` table, then edits the tree (modify, touch, add) and
//! re-scans. Asserts the per-summary cache stats are right (some
//! files reuse, the rest rehash) and snapshots the table again to
//! catch silent regressions in the canonicalization, the symlink
//! handling, or the cascaded `.fsindex.yaml` ignore filter.
//!
//! Update with `cargo insta review` from `datalib/backend`.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::{EntryKind, FingerprintCache};
use datalib_etl::fswalk::StampKind;
use datalib_etl::progress::Progress;
use datalib_etl_fsindex::download::{self, FetchOptions, RawDb};
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

/// Verify the Unison cursor reached the host-local cache correctly:
/// every FILE entry should carry `stamp_kind = inode` on unix with
/// non-NULL `inode` + `dev`. If this fails, `fswalk::decide` will never
/// reuse anything, however unchanged the file is.
///
/// The cursor lives in the fingerprint cache, not the scan store — it
/// is host state, so it is deliberately not versioned or branched. See
/// `datalib_etl::fingerprint_cache`.
async fn assert_inode_stamp_kind(cache: &FingerprintCache, root: &Path) {
    let tree = cache.load_under(root).await.unwrap();
    let files: Vec<&String> = tree
        .paths()
        .filter(|rel| tree.kind(rel) == Some(EntryKind::File))
        .collect();
    assert!(
        !files.is_empty(),
        "no file entries in the fingerprint cache"
    );
    for rel in files {
        let cursor = tree.cursor(rel).expect("a cached entry has a cursor");
        #[cfg(unix)]
        {
            assert_eq!(
                cursor.stamp_kind,
                StampKind::Inode,
                "file {rel:?} has stamp_kind={:?} — expected Inode on unix",
                cursor.stamp_kind
            );
            assert!(
                cursor.inode.is_some(),
                "{rel:?} has stamp_kind=Inode but inode is NULL"
            );
            assert!(
                cursor.dev.is_some(),
                "{rel:?} has stamp_kind=Inode but dev is NULL"
            );
        }
        #[cfg(not(unix))]
        {
            assert_eq!(cursor.stamp_kind, StampKind::NoStamp, "{rel:?} on non-unix");
        }
    }
}

fn fetch_opts(db_path: &Path, root: &Path, cache: FingerprintCache) -> FetchOptions {
    FetchOptions {
        db_path: db_path.to_path_buf(),
        db: None,
        source_id: "smoke".to_string(),
        root: root.to_path_buf(),
        target_doltlite_branch: None,
        cache,
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
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();
    make_initial_tree(&root);

    // ── Phase A: initial scan ───────────────────────────────────────
    let summary_a = download::fetch(fetch_opts(&db_path, &root, cache.clone()))
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
    assert_inode_stamp_kind(&cache, &root).await;

    insta::assert_snapshot!("initial_scan", dump_files(&db_path).await);

    // ── Phase A2: rescan with no changes — Unison fast path ─────────
    // This is the test that the inode-based cursor is actually
    // doing its job. All four FILE rows should reuse their cached
    // blake3 against the unchanged (mtime, size, inode) triple;
    // only the symlink and the two directories should rehash.
    let summary_a2 = download::fetch(fetch_opts(&db_path, &root, cache.clone()))
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

    let summary_b = download::fetch(fetch_opts(&db_path, &root, cache.clone()))
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
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();

    // `subdir` opts into stamping via its own `.fsindex.yaml`; the rest
    // of the tree does not.
    write(&root.join("hello.txt"), b"hello world\n");
    write(
        &root.join("subdir/.fsindex.yaml"),
        b"stamp_me_with_uuid: true\n",
    );
    write(&root.join("subdir/nested.txt"), b"nested");

    let mut opts = fetch_opts(&db_path, &root, cache.clone());
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
    let mut opts2 = fetch_opts(&db_path, &root, cache.clone());
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

/// The cache is keyed by ABSOLUTE path, which is what makes it one
/// chain per host rather than one per root. A relative `--root` must
/// not produce relative keys: the same relative name used from two
/// directories would put two unrelated trees on one key.
///
/// Caught by running the real binary with `--root sub` from two
/// different working directories and finding both trees stored under
/// `sub/...`.
#[tokio::test]
async fn the_cache_is_keyed_absolutely_even_for_a_relative_root() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();

    // Two distinct trees that share a relative name.
    for (tree, body) in [("one", "x"), ("two", "y")] {
        let root = tmp.path().join(tree).join("sub");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), body).unwrap();

        let mut opts = fetch_opts(
            &tmp.path().join(format!("{tree}.doltlite_db")),
            &root,
            cache.clone(),
        );
        // A non-canonical root: the shape that broke it. The original
        // bug was a *relative* `--root`, which cannot be exercised here
        // without `set_current_dir` — racy under a threaded runner — but
        // both go through the same `canonicalize`, and canonicalize
        // always yields an absolute path.
        opts.root = root.join("..").join("sub");
        download::fetch(opts).await.unwrap();
    }

    let rows: Vec<String> = sqlx::query_scalar("SELECT abs_path FROM fingerprints")
        .fetch_all(cache.pool())
        .await
        .unwrap();
    assert!(!rows.is_empty(), "nothing reached the cache");
    for path in &rows {
        assert!(
            path.starts_with('/'),
            "relative key {path:?} in the cache — two trees could collide on it"
        );
    }
    let unique: std::collections::BTreeSet<&String> = rows.iter().collect();
    assert_eq!(
        unique.len(),
        rows.len(),
        "two trees collided on one cache key"
    );
    // Both trees are present and distinct.
    assert_eq!(
        rows.iter().filter(|p| p.ends_with("/sub/f.txt")).count(),
        2,
        "expected one `sub/f.txt` per tree, got {rows:?}"
    );
}

/// A symlinked route to a tree must share the cache with the real one.
///
/// The root is canonicalized, which resolves symlinks fully, so
/// scanning `/x/link` and `/x/real` address one set of entries instead
/// of hashing the same bytes twice. Entries *inside* the tree are
/// deliberately NOT resolved: fsindex records a symlink as a symlink
/// (hashing its target string), so canonicalizing it would conflate it
/// with whatever it points at.
#[tokio::test]
async fn a_symlinked_root_shares_the_cache_with_its_real_path() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir_all(real.join("sub")).unwrap();
    std::fs::write(real.join("sub/f.bin"), b"content").unwrap();
    // A symlink inside the tree, pointing at a sibling.
    std::os::unix::fs::symlink(real.join("sub/f.bin"), real.join("inside.link")).unwrap();

    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();

    // Scan through the symlinked route first.
    let via_link = download::fetch(fetch_opts(
        &tmp.path().join("a.doltlite_db"),
        &link,
        cache.clone(),
    ))
    .await
    .unwrap();
    assert_eq!(via_link.files_hashed, 1, "the first scan should hash");
    assert_eq!(via_link.symlinks, 1, "the in-tree symlink stays a symlink");

    // The real route reuses all of it.
    let via_real = download::fetch(fetch_opts(
        &tmp.path().join("b.doltlite_db"),
        &real,
        cache.clone(),
    ))
    .await
    .unwrap();
    assert_eq!(
        via_real.files_hashed, 0,
        "the real path rehashed what the symlinked path had already cached"
    );
    assert_eq!(via_real.files_reused, 1);

    let keys: Vec<String> = sqlx::query_scalar("SELECT abs_path FROM fingerprints")
        .fetch_all(cache.pool())
        .await
        .unwrap();
    let canonical = real.canonicalize().unwrap();
    for key in &keys {
        assert!(
            key.starts_with(canonical.to_str().unwrap()),
            "entry {key:?} was keyed under the symlink rather than the real path"
        );
    }
    // The in-tree symlink is keyed where it was walked, not where it points.
    assert!(
        keys.iter().any(|k| k.ends_with("/inside.link")),
        "the in-tree symlink was resolved away instead of recorded: {keys:?}"
    );
}

/// A path that is really gone leaves the cache; a path this scan merely
/// filtered out does not.
///
/// Those look identical from inside one scan — neither wrote a row —
/// and treating them the same is what let an early version evict a
/// broad scan's work. One `lstat` tells them apart.
#[tokio::test]
async fn deleted_paths_leave_the_cache_but_filtered_ones_stay() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("kept.bin"), b"a").unwrap();
    std::fs::write(root.join("doomed.bin"), b"b").unwrap();
    std::fs::write(root.join("scratch.tmp"), b"c").unwrap();

    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();
    let db_path = tmp.path().join("s.doltlite_db");
    download::fetch(fetch_opts(&db_path, &root, cache.clone()))
        .await
        .unwrap();
    assert_eq!(cache.count().await.unwrap(), 4, "root + three files");

    // One file really goes; the other is merely filtered out.
    std::fs::remove_file(root.join("doomed.bin")).unwrap();
    std::fs::write(root.join(".fsindex.yaml"), "ignore:\n  - \"*.tmp\"\n").unwrap();
    download::fetch(fetch_opts(&db_path, &root, cache.clone()))
        .await
        .unwrap();

    let keys: Vec<String> = sqlx::query_scalar("SELECT abs_path FROM fingerprints")
        .fetch_all(cache.pool())
        .await
        .unwrap();
    assert!(
        !keys.iter().any(|k| k.ends_with("doomed.bin")),
        "a deleted path stayed in the cache: {keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.ends_with("scratch.tmp")),
        "an ignored-but-present path was evicted: {keys:?}"
    );

    // And the filtered file is still cheap when a later scan wants it.
    std::fs::remove_file(root.join(".fsindex.yaml")).unwrap();
    let after = download::fetch(fetch_opts(&db_path, &root, cache.clone()))
        .await
        .unwrap();
    assert_eq!(
        after.files_hashed, 0,
        "the previously-ignored file had to be rehashed"
    );
}
