//! qmd's stored vectors, read out of its index: one embedding per
//! document for the map, and the documents nearest a query for search.
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
//! a document's chunks are averaged, and the average normalised again;
//! a search divides each chunk's dot product by its length instead.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::qmd::mapping::norm_path;
use crate::qmd::{qmd_index_path, QmdHit};

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
    // Read-only: the file belongs to the qmd steps.
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

/// The `limit` documents nearest `query` by cosine, each at its nearest
/// chunk: an exact score of every chunk `query.model` wrote for the
/// active documents in `collections` (all when `None`) whose path is in
/// `among` (all when `None`, else `norm_path`ed qmd paths). Not qmd's
/// own vector search, which cannot take a document set and reads every
/// vector in the file, live or not, to answer.
pub async fn nearest_documents(
    pool: &SqlitePool,
    query: &QueryVector<'_>,
    collections: Option<&[String]>,
    among: Option<&HashSet<String>>,
    limit: usize,
) -> Result<Vec<QmdHit>> {
    let create: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'vectors_vec'",
    )
    .fetch_optional(pool)
    .await?;
    let Some(create) = create else {
        return Ok(Vec::new());
    };
    check_version(pool).await?;
    let dim = dims_of(&create)
        .with_context(|| format!("no `float[N]` column in `vectors_vec`: {create}"))?;
    if query.vector.len() != dim {
        bail!(
            "the query's embedding has {} dimensions and the index's {dim}",
            query.vector.len()
        );
    }
    let mut q = query.vector.to_vec();
    if !unit(&mut q) {
        bail!("the query's embedding is all zeros");
    }

    let wanted = serde_json::to_string(&collections.unwrap_or_default())?;
    let documents: Vec<(String, String)> = sqlx::query_as(
        "SELECT path, hash FROM documents \
          WHERE active = 1 AND (NOT ?1 OR collection IN (SELECT value FROM json_each(?2)))",
    )
    .bind(collections.is_some())
    .bind(wanted)
    .fetch_all(pool)
    .await?;
    let documents: Vec<(String, String)> = match among {
        Some(among) => documents
            .into_iter()
            .filter(|(path, _)| among.contains(&norm_path(path)))
            .collect(),
        None => documents,
    };
    let mut hashes: Vec<&str> = documents.iter().map(|(_, h)| h.as_str()).collect();
    hashes.sort_unstable();
    hashes.dedup();

    // Where each chunk of those contents sits in the vector storage.
    let chunks: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT cv.hash, cv.pos, r.chunk_id, r.chunk_offset \
           FROM content_vectors cv \
           JOIN vectors_vec_rowids r ON r.id = cv.hash || '_' || cv.seq \
          WHERE cv.model = ?1 AND cv.hash IN (SELECT value FROM json_each(?2))",
    )
    .bind(query.model)
    .bind(serde_json::to_string(&hashes)?)
    .fetch_all(pool)
    .await?;
    let mut in_chunk: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
    for (i, (_, _, chunk, offset)) in chunks.iter().enumerate() {
        in_chunk
            .entry(*chunk)
            .or_default()
            .push((usize::try_from(*offset)?, i));
    }
    let storage_chunks: Vec<i64> = in_chunk.keys().copied().collect();
    let validity: HashMap<i64, Vec<u8>> = sqlx::query_as(
        "SELECT chunk_id, validity FROM vectors_vec_chunks \
          WHERE chunk_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&storage_chunks)?)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    // hash → (score, pos) of its nearest chunk.
    let mut best: HashMap<&str, (f32, i64)> = HashMap::new();
    for &storage_chunk in &storage_chunks {
        let blob: Vec<u8> =
            sqlx::query_scalar("SELECT vectors FROM vectors_vec_vector_chunks00 WHERE rowid = ?")
                .bind(storage_chunk)
                .fetch_one(pool)
                .await
                .with_context(|| format!("read vector chunk {storage_chunk}"))?;
        let bitmap = validity
            .get(&storage_chunk)
            .with_context(|| format!("vector chunk {storage_chunk} has no validity bitmap"))?;
        let floats = floats_of(&blob);
        for &(offset, i) in &in_chunk[&storage_chunk] {
            if !is_live(bitmap, offset) {
                continue;
            }
            let v = floats
                .get(offset * dim..(offset + 1) * dim)
                .with_context(|| {
                    format!("slot {offset} is past the end of vector chunk {storage_chunk}")
                })?;
            let (dot, norm2) = dot_and_norm(v, &q);
            if norm2 == 0.0 || !norm2.is_finite() {
                continue;
            }
            let score = dot / norm2.sqrt();
            let (hash, pos, _, _) = &chunks[i];
            let entry = best.entry(hash.as_str()).or_insert((f32::MIN, *pos));
            if score > entry.0 {
                *entry = (score, *pos);
            }
        }
    }

    let mut ranked: Vec<(&str, &str, f32, i64)> = documents
        .iter()
        .filter_map(|(path, hash)| {
            let &(score, pos) = best.get(hash.as_str())?;
            Some((path.as_str(), hash.as_str(), score, pos))
        })
        .collect();
    ranked.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(limit);

    let shown: Vec<&str> = ranked.iter().map(|r| r.1).collect();
    let bodies: HashMap<String, String> = sqlx::query_as(
        "SELECT hash, doc FROM content WHERE hash IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&shown)?)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(ranked
        .into_iter()
        .map(|(path, hash, score, pos)| QmdHit {
            path: path.to_string(),
            score: f64::from(score),
            snippet: bodies
                .get(hash)
                .map(|body| chunk_snippet(body, pos))
                .unwrap_or_default(),
            docid: String::new(),
            title: String::new(),
        })
        .collect())
}

/// A query's embedding and the model that made it.
pub struct QueryVector<'a> {
    pub model: &'a str,
    pub vector: &'a [f32],
}

/// The lines a chunk starts with, under the header `snippet_match_line`
/// reads, so the hit lands on the message the chunk starts in. `pos` is
/// where qmd's chunker cut, in UTF-16 units of the body as JavaScript
/// held it.
pub fn chunk_snippet(body: &str, pos: i64) -> String {
    let pos = usize::try_from(pos).unwrap_or(0);
    let mut units = 0;
    let at = body
        .char_indices()
        .find(|(_, c)| {
            let here = units >= pos;
            units += c.len_utf16();
            here
        })
        .map_or(body.len(), |(i, _)| i);
    let line = 1 + body[..at].matches('\n').count();
    let text: Vec<&str> = body[at..].lines().take(12).collect();
    format!("@@ -{line},1 @@ (0 before, 0 after)\n{}", text.join("\n"))
}

async fn check_version(pool: &SqlitePool) -> Result<()> {
    let info: HashMap<String, String> = sqlx::query(
        "SELECT key, CAST(value AS TEXT) AS value FROM vectors_vec_info \
          WHERE value IS NOT NULL \
            AND key IN ('CREATE_VERSION_MAJOR', 'CREATE_VERSION_MINOR', 'CREATE_VERSION')",
    )
    .fetch_all(pool)
    .await
    .context("read `vectors_vec_info`")?
    .iter()
    .map(|r| {
        Ok((
            r.try_get::<String, _>("key")?,
            r.try_get::<String, _>("value")?,
        ))
    })
    .collect::<Result<_, sqlx::Error>>()
    .context("decode `vectors_vec_info`")?;
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

/// A storage chunk's slots as floats, one after another.
fn floats_of(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// `(v·q, v·v)`, in one pass. On x86_64 the same loop is compiled a
/// second time for AVX2 and taken when the CPU has it; the baseline
/// target there has only SSE2. ARM's baseline NEON needs no second copy.
pub fn dot_and_norm(v: &[f32], q: &[f32]) -> (f32, f32) {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU was just found to have AVX2.
        return unsafe { dot_and_norm_avx2(v, q) };
    }
    dot_and_norm_portable(v, q)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn dot_and_norm_avx2(v: &[f32], q: &[f32]) -> (f32, f32) {
    dot_and_norm_portable(v, q)
}

/// Sixteen running sums of each, so the compiler can keep them in SIMD
/// registers: one sum would fix the order of every addition, and with
/// it a scalar loop (measured 6x slower on 768 dimensions).
#[inline(always)]
fn dot_and_norm_portable(v: &[f32], q: &[f32]) -> (f32, f32) {
    const LANES: usize = 16;
    let mut dot = [0f32; LANES];
    let mut norm = [0f32; LANES];
    let (vs, v_rest) = v.as_chunks::<LANES>();
    let (qs, q_rest) = q.as_chunks::<LANES>();
    for (a, b) in vs.iter().zip(qs) {
        for i in 0..LANES {
            dot[i] += a[i] * b[i];
            norm[i] += a[i] * a[i];
        }
    }
    let mut d: f32 = dot.iter().sum();
    let mut n: f32 = norm.iter().sum();
    for (a, b) in v_rest.iter().zip(q_rest) {
        d += a * b;
        n += a * a;
    }
    (d, n)
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

    /// `pos` counts UTF-16 units, as qmd's chunker did in JavaScript: an
    /// emoji before the cut is two of them.
    #[test]
    fn a_chunk_snippet_starts_on_the_line_the_chunk_starts_on() {
        let body = "# t\n\u{1F600} one\ntwo\nthree";
        let pos = "# t\n\u{1F600} one\n".encode_utf16().count() as i64;
        assert_eq!(
            chunk_snippet(body, pos),
            "@@ -3,1 @@ (0 before, 0 after)\ntwo\nthree"
        );
        assert_eq!(
            chunk_snippet(body, 0),
            "@@ -1,1 @@ (0 before, 0 after)\n# t\n\u{1F600} one\ntwo\nthree"
        );
    }

    /// Lengths on either side of the sixteen-wide body, the tail
    /// included, agree with the plain sums.
    #[test]
    fn dot_and_norm_agrees_with_the_plain_sums() {
        for len in [0, 1, 15, 16, 17, 768, 771] {
            let v: Vec<f32> = (0..len).map(|i| (i % 7) as f32 - 3.0).collect();
            let q: Vec<f32> = (0..len).map(|i| (i % 5) as f32 * 0.5).collect();
            let dot: f32 = v.iter().zip(&q).map(|(a, b)| a * b).sum();
            let norm: f32 = v.iter().map(|a| a * a).sum();
            assert_eq!(dot_and_norm(&v, &q), (dot, norm), "len {len}");
        }
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
