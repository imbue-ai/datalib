//! The blob store: the one part of a step's data that `datalib-http`
//! reads. Its shape is a contract between the step that writes it and
//! the server that hands its bytes out; README.md sets it out.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub use datalib_runtime::layout::BLOBS_DB;

/// The store's one table: bytes filed under their blake3, written once
/// and never changed — so no stamp either, since when a row arrived is
/// the date of the commit that brought it.
pub const CAS_OBJECTS_DDL: &str = "CREATE TABLE IF NOT EXISTS cas_objects (
    blake3            TEXT PRIMARY KEY,
    byte_len          INTEGER NOT NULL,
    content_type      TEXT NULL,
    bytes             BLOB NOT NULL,
    CHECK (length(blake3) = 64)
)";

/// How long a reader waits out a writer's commit. A commit writes bytes
/// already in memory, so it lasts as long as the disk write.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where a page finds the bytes `blake3` in the blob store of `step`
/// (`<group>/<function>`).
pub fn address(step: &str, blake3: &str) -> String {
    format!("/data/{step}/blob/{blake3}")
}

/// Whether `s` is a key the store could hold: a blake3 in lowercase hex.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The store at `path`, read-only. Nothing to pin: a reader sees
/// committed transactions only, and a row never changes once there.
pub async fn open_reader(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open blob store {}", path.display()))
}

/// One row's bytes, and the type its writer stored with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

pub async fn get(pool: &SqlitePool, blake3: &str) -> Result<Option<Stored>> {
    let row = sqlx::query("SELECT content_type, bytes FROM cas_objects WHERE blake3 = ?")
        .bind(blake3)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("read blob {blake3}"))?;
    row.map(|r| {
        Ok(Stored {
            content_type: r.try_get("content_type")?,
            bytes: r.try_get("bytes")?,
        })
    })
    .transpose()
}

/// One lookup in the store at `path`, opened and closed around it.
/// `None` when there is no store there, or it does not hold `blake3`.
pub async fn read(path: &Path, blake3: &str) -> Result<Option<Stored>> {
    if !path.is_file() {
        return Ok(None);
    }
    let pool = open_reader(path).await?;
    let got = get(&pool, blake3).await;
    pool.close().await;
    got
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store_with(path: &Path, rows: &[(&str, Option<&str>, &[u8])]) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query(CAS_OBJECTS_DDL).execute(&pool).await.unwrap();
        for (hash, ct, bytes) in rows {
            sqlx::query(
                "INSERT INTO cas_objects (blake3, byte_len, content_type, bytes) VALUES (?, ?, ?, ?)",
            )
            .bind(hash)
            .bind(bytes.len() as i64)
            .bind(ct)
            .bind(*bytes)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
    }

    /// A row comes back with the type it was stored under; a hash the
    /// store lacks, or a step with no store at all, is `None` rather
    /// than an error, so the server can answer "not found".
    #[tokio::test]
    async fn reads_a_row_by_hash_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOBS_DB);
        let hash = blake3::hash(b"tea").to_hex().to_string();
        store_with(&path, &[(&hash, Some("text/plain"), b"tea")]).await;

        assert_eq!(
            read(&path, &hash).await.unwrap(),
            Some(Stored {
                content_type: Some("text/plain".into()),
                bytes: b"tea".to_vec()
            })
        );
        let other = blake3::hash(b"earl grey").to_hex().to_string();
        assert_eq!(read(&path, &other).await.unwrap(), None);
        assert_eq!(
            read(&dir.path().join("absent.sqlite"), &hash)
                .await
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_hash_is_64_lowercase_hex_digits() {
        let hash = blake3::hash(b"tea").to_hex().to_string();
        assert!(is_hash(&hash));
        assert!(!is_hash(&hash.to_uppercase()));
        assert!(!is_hash(&hash[1..]));
        assert!(!is_hash(&format!("../{}", &hash[3..])));
    }

    #[test]
    fn an_address_names_the_step_then_the_hash() {
        assert_eq!(address("gmail/ingest", "ab"), "/data/gmail/ingest/blob/ab");
    }
}
