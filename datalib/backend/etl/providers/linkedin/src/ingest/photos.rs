//! The connection photos earlier builds fetched from linkedin.com: bytes
//! in the per-source CAS, mapped by `contact_photos` edge rows; a page
//! links each at its blob-store address. Nothing
//! adds to them now, because linkedin.com shows a profile only to a
//! signed-in visitor; they are kept, rendered, and pruned with their
//! connections.

use anyhow::{Context, Result};
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

/// A stored photo without its bytes: the `contact_photos` row and the
/// hash its bytes are kept under.
#[derive(Debug, Clone)]
pub struct PhotoEdge {
    pub row_id: String,
    pub blake3: String,
}

/// Every stored connection photo, keyed by `owner_id` (the connection's
/// URL). Empty when the store never held one.
pub async fn load_photo_blobs(db: &RawDb) -> Result<std::collections::HashMap<String, PhotoBlob>> {
    let edges = load_photo_edges(db).await?;
    let hashes: Vec<&str> = edges.values().map(|e| e.blake3.as_str()).collect();
    let mut bytes = load_photo_bytes(db, &hashes).await?;
    Ok(edges
        .into_iter()
        .filter_map(|(owner, edge)| {
            let (bytes, content_type) = bytes.remove(&edge.blake3)?;
            Some((
                owner,
                PhotoBlob {
                    row_id: edge.row_id,
                    bytes,
                    content_type,
                },
            ))
        })
        .collect())
}

/// The edge of every connection that has a photo's bytes stored, keyed
/// by `owner_id`, without reading a byte.
pub async fn load_photo_edges(db: &RawDb) -> Result<std::collections::HashMap<String, PhotoEdge>> {
    let pool = db.pool();
    if !table_exists(pool, CONTACT_PHOTOS_TABLE).await? {
        return Ok(Default::default());
    }
    // Audited: the only interpolation is a table name the caller chose --
    // a literal.
    let edges: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, owner_id, blake3 FROM {} WHERE blake3 IS NOT NULL",
        CONTACT_PHOTOS_TABLE
    )))
    .fetch_all(pool)
    .await
    .context("load contact_photos")?;
    Ok(edges
        .into_iter()
        .map(|(row_id, owner_id, blake3)| (owner_id, PhotoEdge { row_id, blake3 }))
        .collect())
}

/// Each photo's bytes and type, by hash. A store whose photos were never
/// fetched has no blob store, and so nothing to return.
pub async fn load_photo_bytes(
    db: &RawDb,
    hashes: &[&str],
) -> Result<std::collections::HashMap<String, (Vec<u8>, Option<String>)>> {
    let Some(cas) = db.cas() else {
        return Ok(Default::default());
    };
    let mut out = std::collections::HashMap::new();
    for chunk in hashes.chunks(datalib_etl::bulk::SQL_CHUNK) {
        let rows = sqlx::query(
            "SELECT blake3, bytes, content_type FROM cas_objects
              WHERE blake3 IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(chunk)?)
        .fetch_all(cas.pool())
        .await
        .context("load photo bytes")?;
        for r in rows {
            out.insert(
                r.get::<String, _>("blake3"),
                (
                    r.get::<Vec<u8>, _>("bytes"),
                    r.get::<Option<String>, _>("content_type"),
                ),
            );
        }
    }
    Ok(out)
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
