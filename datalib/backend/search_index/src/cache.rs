//! The inference cache: a chunk's vector under the model that made it and
//! the hash of the prompt it was shown (`chunk::chunk_hash`). A model is a
//! pure function, so an entry is never wrong, only unused; it lives in a
//! file of its own so that rebuilding the index, or the grid index it may
//! join, never throws a day of embedding away. The `vectors` table knows
//! no document: the chunks filed beside it (`chunked.rs`) name the
//! hashes, and search joins from them to here.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;

const DDL: [&str; 1] = ["CREATE TABLE IF NOT EXISTS vectors (
        model TEXT NOT NULL,
        hash TEXT NOT NULL,
        vector BLOB NOT NULL,
        PRIMARY KEY (model, hash)
    ) WITHOUT ROWID"];

/// How many hashes go into one statement's bound JSON array.
const BATCH: usize = 10_000;

pub struct InferenceCache {
    pub(crate) pool: SqlitePool,
}

impl InferenceCache {
    pub async fn open(path: &Path) -> Result<InferenceCache> {
        let pool = crate::file::open(
            path,
            &[
                (&DDL, crate::file::Owns::File(StoreKind::InferenceCache)),
                (
                    &crate::chunked::DDL,
                    crate::file::Owns::Part(StoreKind::SearchChunks),
                ),
            ],
            std::time::Duration::from_secs(60),
        )
        .await
        .with_context(|| format!("open the inference cache at {}", path.display()))?;
        Ok(InferenceCache { pool })
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Of `hashes`, the ones `model` has no vector for, each once, in the
    /// order given.
    pub async fn missing(&self, model: &str, hashes: &[String]) -> Result<Vec<String>> {
        let mut held = HashSet::new();
        for batch in hashes.chunks(BATCH) {
            let found: Vec<String> = sqlx::query_scalar(
                "SELECT hash FROM vectors WHERE model = ? \
                   AND hash IN (SELECT value FROM json_each(?))",
            )
            .bind(model)
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&self.pool)
            .await
            .context("read the cached hashes")?;
            held.extend(found);
        }
        let mut seen = HashSet::new();
        Ok(hashes
            .iter()
            .filter(|h| !held.contains(*h) && seen.insert(h.as_str()))
            .cloned()
            .collect())
    }

    pub async fn put(&self, model: &str, vectors: &[(String, Vec<f32>)]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for (hash, vector) in vectors {
            sqlx::query("INSERT OR REPLACE INTO vectors (model, hash, vector) VALUES (?, ?, ?)")
                .bind(model)
                .bind(hash)
                .bind(to_blob(vector))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await.context("commit the vectors")
    }

    pub async fn get(&self, model: &str, hashes: &[String]) -> Result<HashMap<String, Vec<f32>>> {
        let mut out = HashMap::new();
        for batch in hashes.chunks(BATCH) {
            let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
                "SELECT hash, vector FROM vectors WHERE model = ? \
                   AND hash IN (SELECT value FROM json_each(?))",
            )
            .bind(model)
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&self.pool)
            .await
            .context("read cached vectors")?;
            for (hash, blob) in rows {
                out.insert(hash, from_blob(&blob)?);
            }
        }
        Ok(out)
    }
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Result<Vec<f32>> {
    let (words, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() {
        bail!("a cached vector of {} bytes, not whole f32s", blob.len());
    }
    Ok(words.iter().map(|w| f32::from_le_bytes(*w)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open() -> (tempfile::TempDir, InferenceCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = InferenceCache::open(&dir.path().join("cache.sqlite"))
            .await
            .unwrap();
        (dir, cache)
    }

    fn h(s: &str) -> String {
        s.to_string()
    }

    #[tokio::test]
    async fn a_vector_is_found_under_its_model_and_hash_only() {
        let (_dir, cache) = open().await;
        cache
            .put("gemma", &[(h("a"), vec![1.0, -2.5])])
            .await
            .unwrap();
        assert_eq!(
            cache
                .missing("gemma", &[h("a"), h("b"), h("b")])
                .await
                .unwrap(),
            [h("b")]
        );
        assert_eq!(cache.missing("other", &[h("a")]).await.unwrap(), [h("a")]);
        let got = cache.get("gemma", &[h("a"), h("b")]).await.unwrap();
        assert_eq!(got, HashMap::from([(h("a"), vec![1.0, -2.5])]));
    }

    #[tokio::test]
    async fn the_cache_outlives_its_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = InferenceCache::open(&path).await.unwrap();
        cache.put("gemma", &[(h("a"), vec![0.5])]).await.unwrap();
        cache.close().await;
        let cache = InferenceCache::open(&path).await.unwrap();
        assert!(cache.missing("gemma", &[h("a")]).await.unwrap().is_empty());
    }
}
