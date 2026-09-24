//! A sink's version, read from the sink rather than taken from the step
//! that wrote it: for a step whose tree holds doltlite stores, the commit
//! each store's `main` is at. What a reader on `main` can see is what a
//! consumer reads, so that is the version it has consumed.
//! `docs/dev/plans/supervisor.md` §2.1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const STORE_SUFFIX: &str = ".doltlite_db";

/// The stores directly in a step's tree, in name order. Only the top
/// level: a render tree's per-document directories hold markdown, and a
/// store is always at the root of the tree that owns it.
pub fn stores_in(tree: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tree) else {
        return Vec::new();
    };
    let mut stores: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(STORE_SUFFIX))
        })
        .collect();
    stores.sort();
    stores
}

/// `<store file>:<main's head>` for each store, space-separated. `None`
/// when the tree has no store: the sink has no version of its own to read,
/// and the step's report is what there is. (A store is born with a commit;
/// `-` marks one read by a build without the dolt extensions.)
pub async fn read_version(tree: &Path) -> Result<Option<String>> {
    let stores = stores_in(tree);
    let mut parts = Vec::with_capacity(stores.len());
    let mut any = false;
    for store in &stores {
        let head = head_of(store)
            .await
            .with_context(|| format!("read the head of {}", store.display()))?;
        any |= head.is_some();
        let name = store
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        parts.push(format!("{name}:{}", head.as_deref().unwrap_or("-")));
    }
    Ok(any.then(|| parts.join(" ")))
}

/// A read-only connection lands on `main`, whatever branch the writer is
/// on, so this is the last commit a writer published.
async fn head_of(store: &Path) -> Result<Option<String>> {
    let pool = datalib_pin::open_reader(store).await?;
    let head = datalib_pin::head(&pool).await;
    pool.close().await;
    Ok(head?.map(|p| p.commit().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A store with one table, committed `commits` times on `main`.
    async fn store(path: &Path, commits: usize) {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..commits {
            sqlx::query("INSERT INTO t VALUES (?)")
                .bind(i as i64)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_commit('-Am', 'c')")
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    async fn commit(path: &Path) {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display())).unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (99)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'more')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn a_tree_without_a_store_has_no_version_to_read() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("page.xml"), "<tei/>").unwrap();
        assert_eq!(read_version(td.path()).await.unwrap(), None);
        assert_eq!(read_version(&td.path().join("absent")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn every_store_at_the_top_of_the_tree_is_named_with_its_head() {
        let td = tempfile::tempdir().unwrap();
        store(&td.path().join("entities.doltlite_db"), 1).await;
        store(&td.path().join("blobs.doltlite_db"), 0).await;
        std::fs::create_dir(td.path().join("doc")).unwrap();
        store(&td.path().join("doc/nested.doltlite_db"), 1).await;

        let v = read_version(td.path()).await.unwrap().unwrap();
        let heads: Vec<(&str, &str)> = v.split(' ').map(|p| p.split_once(':').unwrap()).collect();
        let names: Vec<&str> = heads.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["blobs.doltlite_db", "entities.doltlite_db"], "{v}");
        for (_, hash) in &heads {
            datalib_pin::Pin::at(*hash).expect("a commit hash");
        }
        assert_ne!(heads[0].1, heads[1].1);
    }

    /// A store is born with a commit, so even an empty one has a version.
    /// No test here claims two stores of equal content share one: a commit's
    /// hash covers its timestamp.
    #[tokio::test]
    async fn a_store_has_a_version_from_birth_and_each_commit_moves_it() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("a.doltlite_db");
        store(&path, 0).await;
        let born = read_version(td.path())
            .await
            .unwrap()
            .expect("born with a commit");
        commit(&path).await;
        let after = read_version(td.path()).await.unwrap().unwrap();
        assert_ne!(born, after);
        assert_eq!(read_version(td.path()).await.unwrap().unwrap(), after);
    }
}
