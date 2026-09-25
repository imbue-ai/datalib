//! One embedding per document, read out of the qmd index.
//!
//! qmd keeps its vectors in a sqlite-vec `vec0` table, `vectors_vec`, one
//! row per chunk keyed `<content hash>_<seq>`. A `vec0` table can only be
//! queried with the sqlite-vec extension loaded, which our binaries do not
//! link — but its storage is ordinary tables beside it, and those are
//! read here directly:
//!
//! - `vectors_vec_rowids(id, chunk_id, chunk_offset)` places each key in
//!   a storage chunk;
//! - `vectors_vec_chunks(chunk_id, validity)` holds a bitmap of which
//!   slots of that chunk are live, least significant bit first;
//! - `vectors_vec_vector_chunks00(rowid, vectors)` holds the chunk's
//!   vectors, `dim` little-endian `f32`s per slot.
//!
//! That layout belongs to sqlite-vec 0.1 (qmd 2.8.3 pins 0.1.9), so the
//! version the table records is checked first and anything else is
//! refused. Measured against `vec_to_json` on a real index: every value
//! agrees to the last digit.
//!
//! qmd's vectors are not unit length, so each chunk is normalised before
//! a document's chunks are averaged, and the average normalised again.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::qmd::qmd_index_path;

/// Every active document with at least one embedded chunk: its qmd path
/// (`<group>/render_markdown/…`, the data-root-relative path the grid's
/// `qmd_path` also holds) and its unit vector, row-major.
#[derive(Debug, Default)]
pub struct DocumentVectors {
    pub dim: usize,
    pub paths: Vec<String>,
    pub vectors: Vec<f32>,
    /// Active documents with no embedded chunk yet.
    pub unembedded: usize,
}

/// `None` when the root has no qmd index yet.
pub async fn read_document_vectors(root: &Path) -> Result<Option<DocumentVectors>> {
    let path = qmd_index_path(root);
    if !path.exists() {
        return Ok(None);
    }
    // Read-only: the file belongs to the `qmd_index` step.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let out = read_from(&pool).await;
    pool.close().await;
    out.map(Some)
        .with_context(|| format!("read the vectors in {}", path.display()))
}

pub async fn read_from(pool: &SqlitePool) -> Result<DocumentVectors> {
    let create: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'vectors_vec'",
    )
    .fetch_optional(pool)
    .await?;
    let documents: Vec<(String, String)> = sqlx::query_as(
        "SELECT path, hash FROM documents WHERE active = 1 ORDER BY collection, path",
    )
    .fetch_all(pool)
    .await?;
    // No table is a store nothing has embedded into yet.
    let Some(create) = create else {
        return Ok(DocumentVectors {
            unembedded: documents.len(),
            ..Default::default()
        });
    };
    check_version(pool).await?;
    let dim = dims_of(&create)
        .with_context(|| format!("no `float[N]` column in `vectors_vec`: {create}"))?;

    let mut slot_of_hash: HashMap<&str, usize> = HashMap::new();
    for (_, hash) in &documents {
        let next = slot_of_hash.len();
        slot_of_hash.entry(hash.as_str()).or_insert(next);
    }
    // chunk_id → (offset in the chunk, which hash's sum it adds to).
    let mut in_chunk: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
    let rows = sqlx::query("SELECT id, chunk_id, chunk_offset FROM vectors_vec_rowids")
        .fetch_all(pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let Some((hash, _seq)) = split_hash_seq(&id) else {
            bail!("a `vectors_vec` key not shaped `<hash>_<seq>`: {id:?}");
        };
        let Some(&slot) = slot_of_hash.get(hash) else {
            continue; // a chunk of content no active document has
        };
        let chunk: i64 = row.try_get("chunk_id")?;
        let offset: i64 = row.try_get("chunk_offset")?;
        in_chunk
            .entry(chunk)
            .or_default()
            .push((usize::try_from(offset)?, slot));
    }
    let validity: HashMap<i64, Vec<u8>> =
        sqlx::query_as("SELECT chunk_id, validity FROM vectors_vec_chunks")
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();

    let mut sums = vec![0f32; slot_of_hash.len() * dim];
    let mut counts = vec![0usize; slot_of_hash.len()];
    // A few chunks at a time: each is a thousand vectors, and a large
    // index has thousands of chunks.
    let mut after = i64::MIN;
    loop {
        let page = sqlx::query(
            "SELECT rowid, vectors FROM vectors_vec_vector_chunks00 \
              WHERE rowid > ? ORDER BY rowid LIMIT 8",
        )
        .bind(after)
        .fetch_all(pool)
        .await?;
        let Some(last) = page.last() else { break };
        after = last.try_get("rowid")?;
        for row in page {
            add_chunk(&row, &in_chunk, &validity, dim, &mut sums, &mut counts)?;
        }
    }

    let mut out = DocumentVectors {
        dim,
        ..Default::default()
    };
    for (path, hash) in &documents {
        let slot = slot_of_hash[hash.as_str()];
        let mut v = sums[slot * dim..(slot + 1) * dim].to_vec();
        if counts[slot] == 0 || !unit(&mut v) {
            out.unembedded += 1;
            continue;
        }
        out.paths.push(path.clone());
        out.vectors.extend(v);
    }
    Ok(out)
}

fn add_chunk(
    row: &sqlx::sqlite::SqliteRow,
    in_chunk: &HashMap<i64, Vec<(usize, usize)>>,
    validity: &HashMap<i64, Vec<u8>>,
    dim: usize,
    sums: &mut [f32],
    counts: &mut [usize],
) -> Result<()> {
    let chunk: i64 = row.try_get("rowid")?;
    let Some(entries) = in_chunk.get(&chunk) else {
        return Ok(());
    };
    let bitmap = validity
        .get(&chunk)
        .with_context(|| format!("vector chunk {chunk} has no validity bitmap"))?;
    let blob: &[u8] = row.try_get("vectors")?;
    for &(offset, slot) in entries {
        if !is_live(bitmap, offset) {
            continue;
        }
        let mut v = vector_at(blob, offset, dim)
            .with_context(|| format!("slot {offset} is past the end of vector chunk {chunk}"))?;
        if !unit(&mut v) {
            continue;
        }
        for (s, x) in sums[slot * dim..(slot + 1) * dim].iter_mut().zip(&v) {
            *s += x;
        }
        counts[slot] += 1;
    }
    Ok(())
}

async fn check_version(pool: &SqlitePool) -> Result<()> {
    let info: HashMap<String, String> = sqlx::query(
        "SELECT key, CAST(value AS TEXT) AS value FROM vectors_vec_info \
          WHERE key IN ('CREATE_VERSION_MAJOR', 'CREATE_VERSION_MINOR', 'CREATE_VERSION')",
    )
    .fetch_all(pool)
    .await
    .context("read `vectors_vec_info`")?
    .into_iter()
    .filter_map(|r| Some((r.try_get("key").ok()?, r.try_get("value").ok()?)))
    .collect();
    let major = info.get("CREATE_VERSION_MAJOR").map(String::as_str);
    let minor = info.get("CREATE_VERSION_MINOR").map(String::as_str);
    if (major, minor) != (Some("0"), Some("1")) {
        bail!(
            "`vectors_vec` was written by sqlite-vec {}, and only 0.1's storage \
             layout is known here (see unified_index/src/qmd/vectors.rs)",
            info.get("CREATE_VERSION")
                .map_or("of no recorded version", String::as_str)
        );
    }
    Ok(())
}

/// The dimension in `CREATE VIRTUAL TABLE … USING vec0(…, embedding float[768] …)`.
pub fn dims_of(create_sql: &str) -> Option<usize> {
    let at = create_sql.find("float[")? + "float[".len();
    let rest = &create_sql[at..];
    rest[..rest.find(']')?]
        .trim()
        .parse()
        .ok()
        .filter(|&d| d > 0)
}

/// `<hash>_<seq>`, split at the last underscore.
pub fn split_hash_seq(id: &str) -> Option<(&str, u32)> {
    let (hash, seq) = id.rsplit_once('_')?;
    Some((hash, seq.parse().ok()?)).filter(|(h, _)| !h.is_empty())
}

pub fn is_live(validity: &[u8], offset: usize) -> bool {
    validity
        .get(offset / 8)
        .is_some_and(|byte| byte >> (offset % 8) & 1 == 1)
}

pub fn vector_at(blob: &[u8], offset: usize, dim: usize) -> Option<Vec<f32>> {
    let bytes = blob.get(offset * dim * 4..(offset + 1) * dim * 4)?;
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
    )
}

fn unit(v: &mut [f32]) -> bool {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return false;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dimension_comes_from_the_table_definition() {
        let sql = "CREATE VIRTUAL TABLE vectors_vec USING vec0(hash_seq TEXT PRIMARY KEY, \
                   embedding float[768] distance_metric=cosine)";
        assert_eq!(dims_of(sql), Some(768));
        assert_eq!(
            dims_of("CREATE VIRTUAL TABLE v USING vec0(x float[])"),
            None
        );
        assert_eq!(dims_of("CREATE TABLE t (x)"), None);
    }

    /// qmd's hashes are hex, but the split is at the *last* underscore so
    /// a key is never cut in the wrong place.
    #[test]
    fn a_key_splits_into_hash_and_seq() {
        assert_eq!(split_hash_seq("ab12_0"), Some(("ab12", 0)));
        assert_eq!(split_hash_seq("a_b_17"), Some(("a_b", 17)));
        assert_eq!(split_hash_seq("ab12"), None);
        assert_eq!(split_hash_seq("_3"), None);
        assert_eq!(split_hash_seq("ab_x"), None);
    }

    /// The bitmap as a real index holds it: 228 live slots of 1024 are
    /// 28 bytes of `FF`, then `0F`.
    #[test]
    fn validity_is_least_significant_bit_first() {
        let mut bitmap = vec![0xffu8; 28];
        bitmap.push(0x0f);
        bitmap.resize(128, 0);
        assert!(is_live(&bitmap, 0));
        assert!(is_live(&bitmap, 227));
        assert!(!is_live(&bitmap, 228));
        assert!(!is_live(&bitmap, 5000));
    }

    #[test]
    fn a_slot_is_dim_little_endian_floats() {
        let blob: Vec<u8> = [1.0f32, 2.0, 3.0, -4.5]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        assert_eq!(vector_at(&blob, 1, 2), Some(vec![3.0, -4.5]));
        assert_eq!(vector_at(&blob, 2, 2), None);
    }
}
