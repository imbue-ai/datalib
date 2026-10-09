//! The connection photos earlier builds fetched from linkedin.com: bytes
//! in the per-source CAS, mapped by `contact_photos` edge rows. Nothing
//! adds to them now, because linkedin.com shows a profile only to a
//! signed-in visitor; they are kept, rendered, and pruned with their
//! connections.

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobCas;
use sqlx::Row;

use super::RawDb;

/// The shared contact→photo edge table name (same in the contacts
/// provider). Lives in the entity raw store; bytes live in the CAS.
pub const CONTACT_PHOTOS_TABLE: &str = "contact_photos";

/// Delete the photo edges of connections a clean read of Connections.csv
/// no longer lists, in the transaction that rewrites `connections`.
/// `connections` is that read's row ids, which are the connection keys.
/// Returns how many connections lost their edges.
pub(crate) async fn prune_to_connections_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    connections: &std::collections::HashSet<String>,
) -> Result<usize> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
    )
    .bind(CONTACT_PHOTOS_TABLE)
    .fetch_one(&mut **tx)
    .await
    .context("look for contact_photos")?;
    if !exists {
        return Ok(0);
    }
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT owner_id FROM contact_photos")
        .fetch_all(&mut **tx)
        .await
        .context("load contact_photos owners")?;
    let gone: Vec<String> = owners
        .into_iter()
        .filter(|o| !connections.contains(o))
        .collect();
    for chunk in gone.chunks(datalib_etl::bulk::SQL_CHUNK) {
        let mut sql = String::from("DELETE FROM contact_photos WHERE owner_id IN (");
        datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
        sql.push(')');
        // Audited: the IN-list is a `?,?,?` run sized from the chunk, and
        // every owner is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for owner in chunk {
            q = q.bind(owner);
        }
        q.execute(&mut **tx).await.context("prune contact_photos")?;
    }
    Ok(gone.len())
}

/// A fetched photo: the `contact_photos` row it came through (what a
/// contact declares it read), and the bytes.
#[derive(Debug, Clone)]
pub struct PhotoBlob {
    pub row_id: String,
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

/// Every stored connection photo, keyed by `owner_id` (the connection's
/// URL). Empty when the store never held one.
pub async fn load_photo_blobs(db: &RawDb) -> Result<std::collections::HashMap<String, PhotoBlob>> {
    let pool = db.pool();
    let mut out = std::collections::HashMap::new();
    if !table_exists(pool, CONTACT_PHOTOS_TABLE).await? {
        return Ok(out);
    }

    // owner_id → blake3 for the rows that actually have bytes.
    // Audited: the only interpolation is a table name the caller chose --
    // a literal.
    let edges = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, owner_id, blake3 FROM {} WHERE blake3 IS NOT NULL",
        CONTACT_PHOTOS_TABLE
    )))
    .fetch_all(pool)
    .await
    .context("load contact_photos")?;
    if edges.is_empty() {
        return Ok(out);
    }
    // A reader whose store never had a photo fetched has no CAS file, so
    // there are no bytes to resolve — the edges above say the same thing
    // whenever it is genuinely absent.
    let Some(cas) = db.cas() else {
        return Ok(out);
    };
    let loaded = async {
        for row in edges {
            let row_id: String = row.get("id");
            let owner_id: String = row.get("owner_id");
            let blake3: String = row.get("blake3");
            if let Some((bytes, content_type)) = load_cas_bytes(cas, &blake3).await? {
                out.insert(
                    owner_id,
                    PhotoBlob {
                        row_id,
                        bytes,
                        content_type,
                    },
                );
            }
        }
        Ok(out)
    }
    .await;
    loaded
}

async fn table_exists(pool: &sqlx::SqlitePool, table: &str) -> Result<bool> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name=?")
            .bind(table)
            .fetch_optional(pool)
            .await
            .with_context(|| format!("probe {table}"))?;
    Ok(found.is_some())
}

async fn load_cas_bytes(cas: &BlobCas, blake3: &str) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
        .bind(blake3)
        .fetch_optional(cas.pool())
        .await
        .context("load cas bytes")?;
    Ok(row.map(|r| {
        (
            r.get::<Vec<u8>, _>("bytes"),
            r.get::<Option<String>, _>("content_type"),
        )
    }))
}
