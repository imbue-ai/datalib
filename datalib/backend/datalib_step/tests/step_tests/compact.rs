//! `datalib-dag --compact` over the real `datalib-step`: a render store's
//! history folds to one commit and its content version stays, and a raw
//! store's history is left as it was.

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

async fn commits(path: &Path) -> i64 {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite://{}?mode=ro", path.display()))
        .await
        .unwrap();
    let n = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    n
}

#[tokio::test]
async fn compact_folds_a_render_stores_history_and_leaves_the_raw_stores() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
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
    store_with_history(&render, 6).await;
    store_with_history(&raw, 6).await;
    let version = content_version_at_path(&render).await.unwrap();
    let raw_commits = commits(&raw).await;

    let out = Command::new(bin("DATALIB_DAG_BIN"))
        .arg(root.join("config.toml"))
        .arg("--compact")
        .arg("--binary-dir")
        .arg(root.join("bin"))
        .output()
        .expect("run datalib-dag --compact");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(commits(&render).await, 2, "the first commit and the fold");
    assert_eq!(content_version_at_path(&render).await.unwrap(), version);
    assert!(
        commits(&raw).await >= raw_commits,
        "the raw store's history stays"
    );
    assert!(
        stdout.contains("enterprise/render_markdown") && stdout.contains("in all"),
        "says what it gave back: {stdout}"
    );
}
