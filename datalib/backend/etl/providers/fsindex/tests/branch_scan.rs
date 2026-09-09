//! Two scan roots, one doltlite file, one branch each.

use std::path::Path;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_fsindex::download::{self, FetchOptions, RawDb};
use sqlx::Row;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn opts(
    db: &RawDb,
    root: &Path,
    id: &str,
    branch: Option<&str>,
    cache: FingerprintCache,
) -> FetchOptions {
    FetchOptions {
        db: db.clone(),
        source_id: id.to_string(),
        root: root.to_path_buf(),
        target_doltlite_branch: branch.map(str::to_string),
        cache,
        no_stamp: true,
        progress: Progress::noop(),
        control: DownloadControl::default(),
    }
}

/// Scan `root` into `db_path` on `branch`, then commit — mirroring what
/// `bin/fsindex.rs` does, since `fetch` deliberately leaves the commit
/// to its caller.
async fn scan_and_commit(
    db_path: &Path,
    root: &Path,
    id: &str,
    branch: Option<&str>,
    cache: &FingerprintCache,
) {
    let db = RawDb::open(db_path).await.unwrap();
    if let Some(branch) = branch {
        db.checkout_branch(branch).await.unwrap();
    }
    let o = opts(&db, root, id, branch, cache.clone());
    // `fetch` re-applies the checkout on the same pooled connection;
    // doing it here too matches the binary, which opens the db itself.
    download::fetch(o).await.unwrap();
    db.commit(&format!("scan {id}")).await.unwrap();
    // Closed, not dropped: the next open of this store is a second
    // connection until this one is actually gone.
    db.close().await;
}

async fn files_on_branch(db_path: &Path, branch: &str) -> Vec<String> {
    let db = RawDb::open(db_path).await.unwrap();
    let commit: String = sqlx::query_scalar("SELECT dolt_hashof(?)")
        .bind(branch)
        .fetch_one(db.pool())
        .await
        .unwrap_or_else(|e| panic!("resolve branch {branch:?} to a commit: {e}"));
    // Audited for `AssertSqlSafe`: `commit` is a hex digest that
    // doltlite just produced from `dolt_hashof`, not caller input, and
    // the table-valued function takes it as a literal.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM dolt_at_files('{commit}') WHERE kind = 'file' ORDER BY id"
    )))
    .fetch_all(db.pool())
    .await
    .unwrap();
    let ids = rows
        .iter()
        .map(|r| r.get::<String, _>("id"))
        .collect::<Vec<_>>();
    db.close().await;
    ids
}

/// The regression guard: `--branch` has to actually switch branches.
#[tokio::test]
async fn two_roots_land_on_their_own_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("scans.doltlite_db");
    // A temp cache: never this host's real one.
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();

    let alpha = tmp.path().join("alpha");
    write(&alpha, "shared.txt", "same in both trees\n");
    write(&alpha, "only_alpha.txt", "alpha\n");

    let beta = tmp.path().join("beta");
    write(&beta, "shared.txt", "same in both trees\n");
    write(&beta, "nested/only_beta.txt", "beta\n");

    // First root on the default branch, second on its own.
    scan_and_commit(&db_path, &alpha, "alpha", None, &cache).await;
    scan_and_commit(&db_path, &beta, "beta", Some("beta"), &cache).await;

    // The branch was created.
    let db = RawDb::open(&db_path).await.unwrap();
    let branches = sqlx::query("SELECT name FROM dolt_branches ORDER BY name")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect::<Vec<_>>();
    db.close().await;
    assert!(
        branches.iter().any(|b| b == "beta"),
        "scanning with target_doltlite_branch=beta left no such branch; got {branches:?}"
    );

    // Each branch holds its own root, and only its own.
    let on_main = files_on_branch(&db_path, "main").await;
    let on_beta = files_on_branch(&db_path, "beta").await;
    assert_eq!(
        on_main,
        vec!["only_alpha.txt".to_string(), "shared.txt".to_string()],
        "the second scan should not have touched main"
    );
    assert_eq!(
        on_beta,
        vec!["nested/only_beta.txt".to_string(), "shared.txt".to_string()],
        "the beta branch should hold only the beta root"
    );
}

/// Checking out a branch that already exists must work too — the
/// second scan of the same root is the common case, and a fix that
/// only ever creates would fail on it with "branch already exists".
#[tokio::test]
async fn rescanning_an_existing_branch_reuses_it() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("scans.doltlite_db");
    // A temp cache: never this host's real one.
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
        .await
        .unwrap();
    let root = tmp.path().join("tree");
    write(&root, "a.txt", "one\n");

    scan_and_commit(&db_path, &root, "s", Some("work"), &cache).await;
    write(&root, "b.txt", "two\n");
    scan_and_commit(&db_path, &root, "s", Some("work"), &cache).await;

    let on_work = files_on_branch(&db_path, "work").await;
    assert_eq!(
        on_work,
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "the rescan should have landed on the existing branch"
    );
}

/// `checkout_branch` must fail loudly rather than leaving the caller on
/// whatever branch it happened to be on. A silent no-op here is the
/// dangerous shape: the scan would succeed and write to the wrong
/// branch, which reads as "it worked".
#[tokio::test]
async fn checkout_reports_the_branch_it_actually_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("scans.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();

    db.checkout_branch("feature").await.unwrap();
    let active: String = sqlx::query("SELECT active_branch() AS b")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("b");
    assert_eq!(
        active, "feature",
        "checkout_branch returned Ok but the connection is on {active:?}"
    );
}
