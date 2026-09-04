//! The doltlite side, against real stores.
//!
//! `analyze_test.rs` covers the interpretation with no database in the
//! way; this covers the half that only a real doltlite file can prove —
//! that two independent scan files unify through `file://` remotes and
//! that the prolly diff then works across them.

use std::path::Path;

use datalib_dirtree_diff::store::{self, Commit};

/// Build a scan-shaped store: `files` as fsindex declares it, one
/// commit, and return its HEAD.
async fn make_scan(path: &Path, rows: &[(&str, &str, i64, &str)]) -> Commit {
    let pool = store::open(path).await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS files (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL,
            size INTEGER NOT NULL, blake3 BLOB NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, kind, size, digest) in rows {
        sqlx::query("INSERT INTO files (id, kind, size, blake3) VALUES (?,?,?,?)")
            .bind(id)
            .bind(kind)
            .bind(size)
            .bind(hex::decode(digest).unwrap())
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("SELECT dolt_commit('-Am','scan')")
        .execute(&pool)
        .await
        .unwrap();
    let commit = store::resolve_ref(&pool, "HEAD").await.unwrap();
    pool.close().await;
    commit
}

fn digest(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

/// The headline capability: two files that share no history at all,
/// diffed against each other.
#[tokio::test]
async fn two_independent_files_unify_and_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let left_path = tmp.path().join("before.doltlite_db");
    let right_path = tmp.path().join("after.doltlite_db");

    let left = make_scan(
        &left_path,
        &[
            ("docs", "dir", 30, &digest(0xaa)),
            ("docs/q3.txt", "file", 11, &digest(0xbb)),
            ("gone.txt", "file", 5, &digest(0xcc)),
        ],
    )
    .await;
    let right = make_scan(
        &right_path,
        &[
            // The same subtree, moved.
            ("archive", "dir", 30, &digest(0xaa)),
            ("archive/q3.txt", "file", 11, &digest(0xbb)),
            ("fresh.txt", "file", 7, &digest(0xdd)),
        ],
    )
    .await;
    assert_ne!(left, right, "the two scans must be distinct commits");

    let unified = tmp.path().join("unified.doltlite_db");
    store::unify(&unified, &left_path, &right_path)
        .await
        .unwrap();
    let pool = store::open(&unified).await.unwrap();

    let diff = store::fetch_diff(&pool, &left, &right).await.unwrap();
    let mut removed: Vec<&str> = diff.removed.iter().map(|e| e.path.as_str()).collect();
    let mut added: Vec<&str> = diff.added.iter().map(|e| e.path.as_str()).collect();
    removed.sort_unstable();
    added.sort_unstable();
    assert_eq!(removed, vec!["docs", "docs/q3.txt", "gone.txt"]);
    assert_eq!(added, vec!["archive", "archive/q3.txt", "fresh.txt"]);

    // The move is detectable because the digests survived the transfer.
    let moved_out = diff.removed.iter().find(|e| e.path == "docs").unwrap();
    let moved_in = diff.added.iter().find(|e| e.path == "archive").unwrap();
    assert_eq!(
        moved_out.digest, moved_in.digest,
        "a moved directory must keep its tree-hash across the unify"
    );
    pool.close().await;
}

/// The trap that made the first Rust port fail, recorded so it cannot
/// come back silently.
///
/// doltlite registers the per-table `dolt_diff_<table>` /
/// `dolt_at_<table>` vtabs when a **connection is opened**, from the
/// tables present at that moment. A scratch database is empty when we
/// open it to add remotes, so that connection never learns about
/// `files` — and every query on it fails — while a fresh connection to
/// the very same file works. If doltlite ever starts refreshing the
/// registry, this test fails and `store::unify` can stop closing its
/// pool.
#[tokio::test]
async fn the_fetching_connection_cannot_see_what_it_fetched() {
    let tmp = tempfile::tempdir().unwrap();
    let left_path = tmp.path().join("l.doltlite_db");
    let right_path = tmp.path().join("r.doltlite_db");
    let left = make_scan(&left_path, &[("a.txt", "file", 1, &digest(0x11))]).await;
    let right = make_scan(&right_path, &[("b.txt", "file", 1, &digest(0x22))]).await;

    let scratch = tmp.path().join("scratch.doltlite_db");
    let pool = store::open(&scratch).await.unwrap();
    for (name, path) in [("l", &left_path), ("r", &right_path)] {
        let url = format!("file://{}", std::fs::canonicalize(path).unwrap().display());
        sqlx::query("SELECT dolt_remote('add', ?, ?)")
            .bind(name)
            .bind(&url)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_fetch(?)")
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }

    let on_fetching_connection = store::fetch_diff(&pool, &left, &right).await;
    assert!(
        on_fetching_connection.is_err(),
        "the fetching connection unexpectedly saw `files` — if doltlite now \
         refreshes its vtab registry, drop the reopen in `store::unify`"
    );
    pool.close().await;

    // The same file, a new connection: fine.
    let reopened = store::open(&scratch).await.unwrap();
    let diff = store::fetch_diff(&reopened, &left, &right).await.unwrap();
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.added.len(), 1);
    reopened.close().await;
}

/// Two commits in one file need no unification at all.
#[tokio::test]
async fn two_commits_in_one_file_diff_directly() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scans.doltlite_db");

    let first = make_scan(&path, &[("a.txt", "file", 1, &digest(0x11))]).await;
    let pool = store::open(&path).await.unwrap();
    sqlx::query("INSERT INTO files (id, kind, size, blake3) VALUES ('b.txt','file',2,?)")
        .bind(hex::decode(digest(0x22)).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT dolt_commit('-Am','second')")
        .execute(&pool)
        .await
        .unwrap();
    let second = store::resolve_ref(&pool, "HEAD").await.unwrap();

    let diff = store::fetch_diff(&pool, &first, &second).await.unwrap();
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.added[0].path, "b.txt");
    assert!(diff.removed.is_empty());
    pool.close().await;
}

/// Reads go through a pin, so a dirty working set is invisible.
#[tokio::test]
async fn reads_see_the_commit_not_the_working_set() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scan.doltlite_db");
    let commit = make_scan(&path, &[("a.txt", "file", 1, &digest(0x11))]).await;

    let pool = store::open(&path).await.unwrap();
    // Uncommitted: this lands in the working set only.
    sqlx::query("INSERT INTO files (id, kind, size, blake3) VALUES ('uncommitted.txt','file',9,?)")
        .bind(hex::decode(digest(0x33)).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let pinned = store::load_side(&pool, &commit).await.unwrap();
    let paths: Vec<&str> = pinned.iter().map(|e| e.path.as_str()).collect();
    assert!(
        !paths.contains(&"uncommitted.txt"),
        "a pinned read saw the working set: {paths:?}"
    );
    assert!(paths.contains(&"a.txt"));
    pool.close().await;
}

/// The digest lookup behind "deleted, or moved somewhere else?".
#[tokio::test]
async fn digest_lookup_finds_surviving_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scan.doltlite_db");
    let commit = make_scan(
        &path,
        &[
            ("keep/original.txt", "file", 4, &digest(0x44)),
            ("other.txt", "file", 4, &digest(0x55)),
        ],
    )
    .await;

    let pool = store::open(&path).await.unwrap();
    let want = [digest(0x44).to_uppercase(), digest(0x99).to_uppercase()]
        .into_iter()
        .collect();
    let found = store::lookup_digests(&pool, &commit, &want).await.unwrap();
    assert_eq!(
        found.get(&digest(0x44).to_uppercase()).map(String::as_str),
        Some("keep/original.txt")
    );
    assert!(
        !found.contains_key(&digest(0x99).to_uppercase()),
        "a digest nowhere in the tree must not resolve"
    );
    pool.close().await;
}

/// `duplicate_candidates` filters by size and short-circuits at zero.
#[tokio::test]
async fn duplicate_candidates_respect_the_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scan.doltlite_db");
    let commit = make_scan(
        &path,
        &[
            ("small.bin", "file", 10, &digest(0x66)),
            ("big.bin", "file", 5000, &digest(0x77)),
        ],
    )
    .await;
    let pool = store::open(&path).await.unwrap();

    let all = store::duplicate_candidates(&pool, &commit, 1)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);

    let big_only = store::duplicate_candidates(&pool, &commit, 1000)
        .await
        .unwrap();
    assert_eq!(big_only.len(), 1);
    assert_eq!(big_only[0].path, "big.bin");

    let off = store::duplicate_candidates(&pool, &commit, 0)
        .await
        .unwrap();
    assert!(off.is_empty(), "threshold 0 must skip the scan entirely");
    pool.close().await;
}

/// A ref name only means something inside its own file.
#[tokio::test]
async fn refs_resolve_against_their_own_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scan.doltlite_db");
    let head = make_scan(&path, &[("a.txt", "file", 1, &digest(0x11))]).await;

    let pool = store::open(&path).await.unwrap();
    assert_eq!(store::resolve_ref(&pool, "main").await.unwrap(), head);
    assert_eq!(store::resolve_ref(&pool, "HEAD").await.unwrap(), head);
    assert!(store::resolve_ref(&pool, "no-such-branch").await.is_err());
    pool.close().await;
}
