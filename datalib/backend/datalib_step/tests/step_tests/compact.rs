//! `datalib-dag --free-disk-space` over the real `datalib-step`: alone it
//! folds nothing; with `--drop-old-versions-on-derived-data` it folds a render store's
//! history to one commit and keeps its content version; both give back
//! what no commit reaches, and a raw store keeps its history either way.
//! `--compact`, the old spelling, does what the second does.

use std::path::{Path, PathBuf};
use std::process::Command;

use datalib_etl::doltlite_raw::{commit_run, content_version_at_path, open};

fn bin(var: &str) -> PathBuf {
    Path::new(&std::env::var(var).unwrap_or_else(|_| panic!("{var} is set")))
        .canonicalize()
        .unwrap_or_else(|e| panic!("{var}: {e}"))
}

async fn store_with_history(path: &Path, commits: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = open(
        path,
        &["CREATE TABLE log (id INTEGER PRIMARY KEY, body TEXT)"],
    )
    .await
    .unwrap();
    for stardate in 0..commits {
        sqlx::query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) \
             INSERT OR REPLACE INTO log SELECT i, ? || hex(randomblob(100)) FROM n",
        )
        .bind(format!("captain's log, stardate {stardate}: "))
        .execute(&pool)
        .await
        .unwrap();
        commit_run(&pool, &format!("stardate {stardate}"))
            .await
            .unwrap();
    }
    pool.close().await;
}

/// Rewrites every row in twenty transactions under one commit, so the
/// pages the nineteen in between wrote are reachable from no commit.
async fn leave_garbage(path: &Path) {
    let pool = open(
        path,
        &["CREATE TABLE log (id INTEGER PRIMARY KEY, body TEXT)"],
    )
    .await
    .unwrap();
    for _ in 0..20 {
        sqlx::query("UPDATE log SET body = hex(randomblob(100))")
            .execute(&pool)
            .await
            .unwrap();
    }
    commit_run(&pool, "one commit over twenty transactions")
        .await
        .unwrap();
    pool.close().await;
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// A root with one source, whose raw and render stores each hold six
/// commits and garbage: its render store and its raw store.
async fn enterprise(root: &Path) -> (PathBuf, PathBuf) {
    std::fs::create_dir(root.join("bin")).unwrap();
    std::os::unix::fs::symlink(bin("DATALIB_STEP_BIN"), root.join("bin/datalib-step")).unwrap();
    std::fs::write(
        root.join("config.toml"),
        "[[groups]]\nid = \"enterprise\"\ntype = \"slack\"\n\n\
         [[steps]]\ngroup = \"enterprise\"\nfunction = \"ingest\"\n\n\
         [[steps]]\ngroup = \"enterprise\"\nfunction = \"render_markdown\"\n\
         inputs = [\"enterprise/ingest\"]\n",
    )
    .unwrap();
    let render = root.join("enterprise/render_markdown/indexed_markdown.doltlite_db");
    let raw = datalib_etl::raw_layout::entities_db(&root.join("enterprise/ingest"));
    for store in [&render, &raw] {
        store_with_history(store, 6).await;
        leave_garbage(store).await;
    }
    (render, raw)
}

/// `datalib-dag <flags>` on the root, and what it printed.
fn dag(root: &Path, flags: &[&str]) -> String {
    let out = Command::new(bin("DATALIB_DAG_BIN"))
        .arg(root.join("config.toml"))
        .args(flags)
        .arg("--binary-dir")
        .arg(root.join("bin"))
        .output()
        .unwrap_or_else(|e| panic!("run datalib-dag {flags:?}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

async fn commits(path: &Path) -> Vec<String> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&format!("sqlite://{}?mode=ro", path.display()))
        .await
        .unwrap();
    let log = sqlx::query_scalar("SELECT commit_hash FROM dolt_log()")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    log
}

/// Every commit `before` named is still in `after`. The launch's migrate
/// pass may add one on top, applying the ingest's own schema to the test's.
fn kept_every_commit(before: &[String], after: &[String]) -> bool {
    before.iter().all(|c| after.contains(c))
}

#[tokio::test]
async fn dropping_old_versions_folds_a_render_store_and_collects_the_raw_store() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let (render, raw) = enterprise(root).await;
    let version = content_version_at_path(&render).await.unwrap();
    let (raw_commits, raw_size) = (commits(&raw).await, size(&raw));

    let stdout = dag(
        root,
        &["--free-disk-space", "--drop-old-versions-on-derived-data"],
    );

    assert_eq!(
        commits(&render).await.len(),
        2,
        "the first commit and the fold"
    );
    assert_eq!(content_version_at_path(&render).await.unwrap(), version);
    assert!(
        kept_every_commit(&raw_commits, &commits(&raw).await),
        "the raw store's history stays"
    );
    assert!(size(&raw) < raw_size, "the raw store is collected");
    assert!(
        stdout.contains("enterprise/render_markdown") && stdout.contains("in all"),
        "says what it gave back: {stdout}"
    );
}

/// Freeing disk space loses nothing: every store keeps every commit, a
/// render store's included, and only what no commit reached goes.
#[tokio::test]
async fn freeing_disk_keeps_every_commit_and_gives_back_the_garbage() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let (render, raw) = enterprise(root).await;
    let before = [
        (commits(&render).await, size(&render)),
        (commits(&raw).await, size(&raw)),
    ];
    let version = content_version_at_path(&render).await.unwrap();

    let stdout = dag(root, &["--free-disk-space"]);

    for (store, (was_commits, was_size)) in [&render, &raw].into_iter().zip(before) {
        assert!(
            kept_every_commit(&was_commits, &commits(store).await),
            "{} keeps its history",
            store.display()
        );
        assert!(size(store) < was_size, "{} is collected", store.display());
    }
    assert_eq!(content_version_at_path(&render).await.unwrap(), version);
    assert!(
        stdout.contains("in all"),
        "says what it gave back: {stdout}"
    );
}

/// `--compact`, the spelling scripts used before `--free-disk-space`,
/// still folds a render store's history.
#[tokio::test]
async fn compact_is_the_old_spelling_of_dropping_old_versions() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let (render, _) = enterprise(root).await;

    dag(root, &["--compact"]);

    assert_eq!(commits(&render).await.len(), 2, "folded");
}
