//! qmd's `index.sqlite`, read from Rust: the index's content version,
//! and the per-collection embedding gauge qmd's own `embed` decides by.
//! Read-only — qmd writes the file; `qmd update` and `qmd embed` are
//! the writers, and this crate only spawns them.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// The busy timeout qmd itself opens the store with (`src/db.ts`), so a
/// reader here waits out a batch commit the way a second qmd would.
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn open_ro(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open qmd index {} read-only", path.display()))
}

/// The content version of the whole index: a digest over every active
/// document's `(collection, path, hash)` and the qmd pin. Stable across
/// runs that change nothing; moves when a document does, or when a qmd
/// bump means the same rows would be read differently — which is what
/// makes every `qmd_embed` step stale after a bump, the way a
/// `LAYOUT_VERSION` bump makes every render stale.
pub async fn index_version(pool: &SqlitePool, qmd_version: &str) -> Result<String> {
    let rows = sqlx::query(
        "SELECT collection, path, hash FROM documents WHERE active = 1 \
         ORDER BY collection, path",
    )
    .fetch_all(pool)
    .await?;
    let mut h = Sha256::new();
    h.update(format!("qmd={qmd_version}\n").as_bytes());
    for r in &rows {
        for col in ["collection", "path", "hash"] {
            h.update(r.try_get::<String, _>(col)?.as_bytes());
            h.update(b"\0");
        }
        h.update(b"\n");
    }
    Ok(hex(&h.finalize()))
}

/// How much of one collection semantic search can reach, by qmd's own
/// definition (`getHashesNeedingEmbedding` in `store.ts`): a document is
/// pending until every chunk of its hash has a vector under the model
/// and fingerprint qmd is currently writing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbedGauge {
    pub active: u64,
    pub pending: u64,
    pub chunks: u64,
}

impl EmbedGauge {
    pub fn embedded(&self) -> u64 {
        self.active.saturating_sub(self.pending)
    }
}

/// Which `(model, embed_fingerprint)` qmd is writing is only knowable
/// from the rows it wrote: the newest one. Before the first vector ever
/// lands there is nothing to read, and a document with no vectors at
/// all is pending under any fingerprint — so that is what the cold
/// answer counts. It cannot see a fingerprint change until the first
/// new-fingerprint row lands, at which point the gauge jumps to the
/// right number; `qmd embed` itself is never misled, because it
/// computes the fingerprint rather than reading it.
pub async fn embed_gauge(pool: &SqlitePool, collection: &str) -> Result<EmbedGauge> {
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE collection = ? AND active = 1")
            .bind(collection)
            .fetch_one(pool)
            .await?;
    let current = sqlx::query(
        "SELECT model, embed_fingerprint FROM content_vectors ORDER BY rowid DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let pending: i64 = match current {
        Some(r) => {
            let model: String = r.try_get("model")?;
            let fingerprint: String = r.try_get("embed_fingerprint")?;
            sqlx::query_scalar(
                "SELECT COUNT(DISTINCT d.hash) FROM documents d \
                 LEFT JOIN (SELECT hash, COUNT(*) AS chunk_count, MAX(total_chunks) AS expected_chunks \
                            FROM content_vectors WHERE model = ? AND embed_fingerprint = ? \
                            GROUP BY hash) v ON d.hash = v.hash \
                 WHERE d.active = 1 AND d.collection = ? \
                   AND (v.hash IS NULL OR v.chunk_count < v.expected_chunks)",
            )
            .bind(&model)
            .bind(&fingerprint)
            .bind(collection)
            .fetch_one(pool)
            .await?
        }
        None => {
            sqlx::query_scalar(
                "SELECT COUNT(DISTINCT d.hash) FROM documents d \
                 WHERE d.active = 1 AND d.collection = ? \
                   AND NOT EXISTS (SELECT 1 FROM content_vectors v WHERE v.hash = d.hash)",
            )
            .bind(collection)
            .fetch_one(pool)
            .await?
        }
    };
    let chunks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM content_vectors v \
         WHERE v.hash IN (SELECT hash FROM documents WHERE collection = ? AND active = 1)",
    )
    .bind(collection)
    .fetch_one(pool)
    .await?;
    Ok(EmbedGauge {
        active: active as u64,
        pending: pending as u64,
        chunks: chunks as u64,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
