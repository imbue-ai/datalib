//! What a search reads: the messages one term matches, and every chunk's
//! vector with the messages it covers. Read-only, from files a step may
//! be writing; each read is one snapshot of each. A document's chunks
//! count only when they were cut from the rendering the keyword index
//! holds: their sections are numbered in it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// How many hashes go into one statement's bound JSON array.
const BATCH: usize = 10_000;

pub struct Reader {
    index: SqlitePool,
    cache: Option<SqlitePool>,
}

/// Every document whose chunks were cut from the rendering the keyword
/// index holds, by uuid, each with the chunks a model has a vector for.
/// A document is built from its rendering (`content_hash`, which the
/// sections and the chunks follow from) and its chunks' vectors (each a
/// pure function of its chunk's hash), so a later read shares every
/// document whose rendering has not moved and that had no chunk waiting
/// for a vector, and builds only the rest, reading from the cache only
/// the vectors their earlier versions did not hold.
#[derive(Debug, Default, PartialEq)]
pub struct Held {
    pub dim: usize,
    pub docs: BTreeMap<String, Arc<HeldDoc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeldDoc {
    pub markdown_uuid: String,
    pub source_id: String,
    /// Each section's message uuid, by `seq`.
    pub sections: Vec<Option<String>>,
    /// Its chunks that have a vector.
    pub chunks: Vec<HeldChunk>,
    /// One unit vector per chunk, laid end to end, in half precision
    /// ([`crate::half`]): half the memory of the f32 the cache stores.
    pub vectors: Vec<u16>,
    content_hash: String,
    /// Each chunk's hash, by its place in `chunks`.
    hashes: Vec<String>,
    /// The hashes of its chunks the cache had no vector for.
    missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldChunk {
    pub start: i64,
    pub end: i64,
    pub first_section: i64,
    pub last_section: i64,
}

impl HeldDoc {
    pub fn vector(&self, chunk: usize) -> &[u16] {
        let dim = self.vectors.len() / self.chunks.len();
        &self.vectors[chunk * dim..(chunk + 1) * dim]
    }

    /// The message uuids chunk `chunk` covers.
    pub fn covered(&self, chunk: usize) -> impl Iterator<Item = &str> {
        let c = &self.chunks[chunk];
        let from = usize::try_from(c.first_section).unwrap_or(0);
        let to = usize::try_from(c.last_section + 1).unwrap_or(0);
        self.sections
            .get(from..to.min(self.sections.len()))
            .unwrap_or_default()
            .iter()
            .filter_map(Option::as_deref)
    }
}

impl Held {
    /// The documents with at least one chunk embedded.
    pub fn embedded(&self) -> impl Iterator<Item = &HeldDoc> {
        self.docs
            .values()
            .map(Arc::as_ref)
            .filter(|d| !d.chunks.is_empty())
    }

    /// How many chunks have a vector.
    pub fn chunks(&self) -> usize {
        self.docs.values().map(|d| d.chunks.len()).sum()
    }

    /// Each embedded document's place in meaning: the mean of its chunks'
    /// unit vectors, scaled to unit length, laid end to end, with its id.
    pub fn document_vectors(&self) -> (Vec<String>, Vec<f32>) {
        let mut ids = Vec::new();
        let mut out = Vec::new();
        for doc in self.embedded() {
            let mut sum = vec![0f32; self.dim];
            for chunk in 0..doc.chunks.len() {
                for (s, x) in sum.iter_mut().zip(doc.vector(chunk)) {
                    *s += crate::half::to_f32(*x);
                }
            }
            if unit(&mut sum) {
                ids.push(doc.markdown_uuid.clone());
                out.extend(sum);
            }
        }
        (ids, out)
    }
}

const DOCUMENTS_OF_SOURCE: &str =
    "SELECT markdown_uuid, content_hash FROM search_documents WHERE source_id = ?";

/// What [`Reader::coverage_of`] reads: one source's documents, or every
/// one. Two statements, not `?1 IS NULL OR source_id = ?1`: that one
/// cannot use the index, and the holdings report asks once per source.
pub(crate) fn held_documents_sql(source_id: Option<&str>) -> &'static str {
    match source_id {
        Some(_) => DOCUMENTS_OF_SOURCE,
        None => "SELECT markdown_uuid, content_hash FROM search_documents",
    }
}

impl Reader {
    /// `None` when no step has written the index yet: no file, or a grid
    /// index's file without the index's tables. A missing cache reads as
    /// one holding no vectors.
    pub async fn open(index: &Path, cache: &Path) -> Result<Option<Reader>> {
        if !index.exists() {
            return Ok(None);
        }
        let index = connect(index).await?;
        let written: Result<Option<i64>, sqlx::Error> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'search_state'",
        )
        .fetch_optional(&index)
        .await;
        match written {
            Ok(Some(_)) => {}
            Ok(None) => {
                index.close().await;
                return Ok(None);
            }
            Err(e) => {
                index.close().await;
                return Err(e).context("look for the search index's tables");
            }
        }
        let cache = if cache.exists() {
            Some(connect(cache).await?)
        } else {
            None
        };
        Ok(Some(Reader { index, cache }))
    }

    pub async fn close(self) {
        self.index.close().await;
        if let Some(cache) = self.cache {
            cache.close().await;
        }
    }

    /// Whether the index's tables are in this build's shape. A writer in
    /// this build empties tables in another, so every source's keyword
    /// pass must run again, which the launch's migrate pass asks for.
    pub async fn in_this_shape(&self) -> Result<bool> {
        let recorded: Option<String> =
            sqlx::query_scalar("SELECT value FROM _datalib_meta WHERE key = ?")
                .bind(datalib_store_meta::part_key(
                    datalib_store_meta::StoreKind::SearchIndex,
                ))
                .fetch_optional(&self.index)
                .await
                .context("read the search index's shape")?;
        Ok(recorded.as_deref() == Some(crate::store::shape().as_str()))
    }

    /// Whether any of `source_id`'s documents was drawn by another
    /// version of the drawing or the chunking than this build's: what the
    /// launch's migrate pass asks, since a new version alone moves nothing
    /// the keyword pass reads.
    pub async fn drawn_by_another_version(&self, source_id: &str) -> Result<bool> {
        let other: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM search_documents \
              WHERE source_id = ?1 AND content_hash NOT LIKE '%' || ?2 LIMIT 1",
        )
        .bind(source_id)
        .bind(crate::text::key_suffix())
        .fetch_optional(&self.index)
        .await
        .context("look for documents drawn by another version")?;
        Ok(other.is_some())
    }

    /// `markdown_uuid → content_hash` for every document of `source_id`
    /// the keyword index holds: what its `embed` cuts into chunks.
    pub async fn documents_of(&self, source_id: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(DOCUMENTS_OF_SOURCE)
            .bind(source_id)
            .fetch_all(&self.index)
            .await
            .context("read the indexed documents")?;
        Ok(rows.into_iter().collect())
    }

    /// Whether `source_id`'s last keyword pass finished: until it does,
    /// a document the index lacks may be one the pass has not reached.
    pub async fn pass_finished(&self, source_id: &str) -> Result<bool> {
        let row: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM search_sources WHERE source_id = ?")
                .bind(source_id)
                .fetch_optional(&self.index)
                .await
                .context("read whether the source's keyword pass finished")?;
        Ok(row.is_some())
    }

    /// How many documents the index holds, embedded or not.
    pub async fn documents(&self) -> Result<usize> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_documents")
            .fetch_one(&self.index)
            .await
            .context("count the indexed documents")?;
        Ok(usize::try_from(n)?)
    }

    /// Of `uuids`, the ones the index holds, each with whether it is
    /// embedded: cut from the rendering the index holds, and every one of
    /// its chunks with a vector under `model`.
    pub async fn states(&self, model: &str, uuids: &[String]) -> Result<HashMap<String, bool>> {
        let mut held: Vec<(String, String)> = Vec::new();
        for batch in uuids.chunks(BATCH) {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT markdown_uuid, content_hash FROM search_documents \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&self.index)
            .await
            .context("read the documents")?;
            held.extend(rows);
        }
        let asked: Vec<String> = held.iter().map(|(uuid, _)| uuid.clone()).collect();
        self.embedded(model, held, Some(&asked)).await
    }

    /// How many documents the index holds, and of them how many are
    /// embedded under `model` ([`Reader::states`]).
    pub async fn coverage(&self, model: &str) -> Result<(u64, u64)> {
        self.coverage_of(model, None).await
    }

    /// [`Reader::coverage`] of one source's documents, or every one.
    pub async fn coverage_of(&self, model: &str, source_id: Option<&str>) -> Result<(u64, u64)> {
        let mut query = sqlx::query_as(held_documents_sql(source_id));
        if let Some(source_id) = source_id {
            query = query.bind(source_id);
        }
        let held: Vec<(String, String)> = query
            .fetch_all(&self.index)
            .await
            .context("read the documents")?;
        let documents = held.len() as u64;
        let embedded = match source_id {
            None => self.embedded(model, held, None).await?,
            Some(_) => {
                let asked: Vec<String> = held.iter().map(|(uuid, _)| uuid.clone()).collect();
                self.embedded(model, held, Some(&asked)).await?
            }
        };
        Ok((
            documents,
            embedded.values().filter(|ok| **ok).count() as u64,
        ))
    }

    /// Whether each of `held`, `(markdown_uuid, content_hash)`, is
    /// embedded; `asked` narrows what is read of the cache.
    async fn embedded(
        &self,
        model: &str,
        held: Vec<(String, String)>,
        asked: Option<&[String]>,
    ) -> Result<HashMap<String, bool>> {
        let filed = match &self.cache {
            Some(cache) => crate::chunked::read_filed(cache, asked).await?,
            None => BTreeMap::new(),
        };
        let mut hashes: Vec<&str> = filed
            .values()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.hash.as_str()))
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        let have = self.vectors_held(model, &hashes).await?;
        Ok(held
            .into_iter()
            .map(|(uuid, content_hash)| {
                let embedded = filed.get(&uuid).is_some_and(|(cut_from, chunks)| {
                    *cut_from == content_hash
                        && chunks.iter().all(|c| have.contains(c.hash.as_str()))
                });
                (uuid, embedded)
            })
            .collect())
    }

    /// Which of `hashes` the cache has a vector for under `model`.
    async fn vectors_held(&self, model: &str, hashes: &[&str]) -> Result<HashSet<String>> {
        let Some(cache) = &self.cache else {
            return Ok(HashSet::new());
        };
        let mut out = HashSet::with_capacity(hashes.len());
        for batch in hashes.chunks(BATCH) {
            let rows: Vec<String> = sqlx::query_scalar(
                "SELECT hash FROM vectors WHERE model = ? \
                   AND hash IN (SELECT value FROM json_each(?))",
            )
            .bind(model)
            .bind(serde_json::to_string(batch)?)
            .fetch_all(cache)
            .await
            .context("look up the cached vectors")?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Raised by every write that changes what the index holds.
    pub async fn generation(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT generation FROM search_state WHERE id = 1")
            .fetch_one(&self.index)
            .await
            .context("read the search index's generation")
    }

    /// Every chunk `model` has a vector for, with its document. A chunk the
    /// cache has no vector for yet is left out: it has not been embedded.
    pub async fn held(&self, model: &str) -> Result<Held> {
        self.held_after(model, None).await
    }

    /// [`Reader::held`], sharing each document of `before`, read earlier
    /// under `model`, that nothing it was built from has moved for
    /// ([`Held`]), and reading the others.
    pub async fn held_after(&self, model: &str, before: Option<&Held>) -> Result<Held> {
        let started = std::time::Instant::now();
        let Some(cache) = &self.cache else {
            return Ok(Held::default());
        };
        // Nothing embedded yet is the usual state until a sync turns
        // embedding on.
        let width: Option<i64> =
            sqlx::query_scalar("SELECT length(vector) FROM vectors WHERE model = ? LIMIT 1")
                .bind(model)
                .fetch_optional(cache)
                .await
                .context("look for a cached vector")?;
        let Some(width) = width else {
            return Ok(Held::default());
        };
        let dim = usize::try_from(width / 4)?;
        if width <= 0 || width % 4 != 0 {
            bail!("a cached vector of {width} bytes, not whole f32s");
        }
        if before.is_some_and(|b| b.dim != 0 && b.dim != dim) {
            bail!(
                "cached vectors of {dim} dimensions, and {} held",
                before.map_or(0, |b| b.dim)
            );
        }
        let docs: Vec<DocRow> = sqlx::query_as(
            "SELECT doc_id, markdown_uuid, source_id, content_hash FROM search_documents",
        )
        .fetch_all(&self.index)
        .await
        .context("read the indexed documents")?;
        let cut = crate::chunked::read_cut(cache).await?;
        let unmoved = |row: &DocRow| {
            let (_, uuid, source_id, content_hash) = row;
            let d = before?.docs.get(uuid)?;
            (d.source_id == *source_id && d.content_hash == *content_hash).then_some(d)
        };
        let mut held = Held {
            dim,
            docs: BTreeMap::new(),
        };
        let mut build: Vec<(DocRow, Option<&Arc<HeldDoc>>)> = Vec::new();
        let mut waiting: Vec<(DocRow, &Arc<HeldDoc>)> = Vec::new();
        let mut listed = 0;
        for row in docs {
            if cut.get(&row.1) != Some(&row.3) {
                continue;
            }
            listed += 1;
            match unmoved(&row) {
                Some(d) if d.missing.is_empty() => {
                    held.docs.insert(row.1, d.clone());
                }
                Some(d) => waiting.push((row, d)),
                None => {
                    let earlier = before.and_then(|b| b.docs.get(&row.1));
                    build.push((row, earlier));
                }
            }
        }
        let kept = held.docs.len();
        let asked: Vec<&str> = waiting
            .iter()
            .flat_map(|(_, d)| d.missing.iter().map(String::as_str))
            .collect();
        let arrived = self.vectors_held(model, &asked).await?;
        for (row, d) in waiting {
            if d.missing.iter().any(|h| arrived.contains(h)) {
                build.push((row, Some(d)));
            } else {
                held.docs.insert(row.1, d.clone());
            }
        }
        let built = build.len();
        let whole = built * 2 >= listed;
        for doc in self.build(cache, model, dim, &build, whole).await? {
            held.docs.insert(doc.markdown_uuid.clone(), Arc::new(doc));
        }
        tracing::info!(
            kept,
            built,
            whole,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "read the held vectors"
        );
        Ok(held)
    }

    /// `docs` as [`HeldDoc`]s: their sections from the index, their chunks
    /// from the cache, and each chunk's vector from the document's earlier
    /// version where it held one, else from the cache. For most of the
    /// library (`whole`) the cache's vectors are read in one pass in key
    /// order, which reads the file in order; for a few, by key.
    async fn build(
        &self,
        cache: &SqlitePool,
        model: &str,
        dim: usize,
        docs: &[(DocRow, Option<&Arc<HeldDoc>>)],
        whole: bool,
    ) -> Result<Vec<HeldDoc>> {
        let mut earlier: HashMap<&str, &[u16]> = HashMap::new();
        for d in docs.iter().filter_map(|(_, d)| *d) {
            for (chunk, hash) in d.hashes.iter().enumerate() {
                earlier.insert(hash.as_str(), d.vector(chunk));
            }
        }
        let rows: Vec<&DocRow> = docs.iter().map(|(row, _)| row).collect();
        let mut sections_of = self.sections_of(&rows, whole).await?;
        let mut filed = if whole {
            crate::chunked::read_filed(cache, None).await?
        } else {
            let uuids: Vec<String> = rows.iter().map(|(_, uuid, ..)| uuid.clone()).collect();
            crate::chunked::read_filed(cache, Some(&uuids)).await?
        };
        let mut building: Vec<Building> = Vec::with_capacity(rows.len());
        let mut filed_of: Vec<Vec<crate::FiledChunk>> = Vec::with_capacity(rows.len());
        for (doc_id, markdown_uuid, source_id, content_hash) in rows {
            let Some((_, chunks)) = filed.remove(markdown_uuid) else {
                continue;
            };
            let mut found = vec![Found::Not; chunks.len()];
            let mut vectors = vec![0u16; chunks.len() * dim];
            for (i, c) in chunks.iter().enumerate() {
                if let Some(v) = earlier.get(c.hash.as_str()).filter(|v| v.len() == dim) {
                    vectors[i * dim..(i + 1) * dim].copy_from_slice(v);
                    found[i] = Found::Unit;
                }
            }
            building.push(Building {
                doc: HeldDoc {
                    markdown_uuid: markdown_uuid.clone(),
                    source_id: source_id.clone(),
                    sections: sections_of.remove(doc_id).unwrap_or_default(),
                    chunks: Vec::new(),
                    vectors,
                    content_hash: content_hash.clone(),
                    hashes: Vec::new(),
                    missing: Vec::new(),
                },
                found,
            });
            filed_of.push(chunks);
        }
        drop(filed);
        let mut wanted: Vec<(&str, u32, u32)> = Vec::new();
        for (d, (b, chunks)) in building.iter().zip(&filed_of).enumerate() {
            for (c, chunk) in chunks.iter().enumerate() {
                if b.found[c] == Found::Not {
                    wanted.push((chunk.hash.as_str(), u32::try_from(d)?, u32::try_from(c)?));
                }
            }
        }
        wanted.sort_unstable();
        let mut places: Vec<(usize, usize, Found)> = Vec::new();
        let mut halves: Vec<u16> = Vec::new();
        let mut take = |hash: &str, blob: &[u8]| -> Result<()> {
            let from = wanted.partition_point(|(h, ..)| *h < hash);
            let to = from + wanted[from..].partition_point(|(h, ..)| *h == hash);
            if from == to {
                return Ok(());
            }
            if blob.len() != dim * 4 {
                bail!(
                    "cached vectors of {} and {} bytes under one model",
                    dim * 4,
                    blob.len()
                );
            }
            let mut v: Vec<f32> = blob
                .as_chunks::<4>()
                .0
                .iter()
                .map(|w| f32::from_le_bytes(*w))
                .collect();
            let found = if unit(&mut v) {
                Found::Unit
            } else {
                Found::Zero
            };
            halves.clear();
            halves.extend(v.iter().map(|x| crate::half::from_f32(*x)));
            for (_, d, c) in &wanted[from..to] {
                places.push((*d as usize, *c as usize, found));
            }
            for (d, c, found) in places.drain(..) {
                let b = &mut building[d];
                b.found[c] = found;
                b.doc.vectors[c * dim..(c + 1) * dim].copy_from_slice(&halves);
            }
            Ok(())
        };
        if whole {
            scan_vectors(cache, model, &mut take).await?;
        } else {
            let mut hashes: Vec<&str> = wanted.iter().map(|(h, ..)| *h).collect();
            hashes.dedup();
            vectors_by_key(cache, model, &hashes, &mut take).await?;
        }
        building
            .into_iter()
            .zip(filed_of)
            .map(|(b, filed)| b.finish(filed, dim))
            .collect()
    }

    /// Each of `docs`' sections' message uuids, by `seq`: for most of the
    /// index (`whole`) read in one pass, for a few by key.
    async fn sections_of(
        &self,
        docs: &[&DocRow],
        whole: bool,
    ) -> Result<HashMap<i64, Vec<Option<String>>>> {
        let ids: HashSet<i64> = docs.iter().map(|(id, ..)| *id).collect();
        let mut out: HashMap<i64, Vec<Option<String>>> = HashMap::new();
        let mut add = |rows: Vec<(i64, Option<String>)>| {
            for (doc, uuid) in rows.into_iter().filter(|(doc, _)| ids.contains(doc)) {
                out.entry(doc).or_default().push(uuid);
            }
        };
        if whole {
            add(
                sqlx::query_as("SELECT doc_id, uuid FROM search_sections ORDER BY doc_id, seq")
                    .fetch_all(&self.index)
                    .await
                    .context("read the indexed sections")?,
            );
        } else {
            let ids: Vec<i64> = ids.iter().copied().collect();
            for batch in ids.chunks(BATCH) {
                add(sqlx::query_as(
                    "SELECT doc_id, uuid FROM search_sections \
                      WHERE doc_id IN (SELECT value FROM json_each(?)) ORDER BY doc_id, seq",
                )
                .bind(serde_json::to_string(batch)?)
                .fetch_all(&self.index)
                .await
                .context("read the indexed sections")?);
            }
        }
        Ok(out)
    }
}

/// Whether a chunk's vector was found, and whether it points anywhere: a
/// vector of zeros is near no query and is held by no chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Not,
    Unit,
    Zero,
}

/// A document being built: a slot per filed chunk, filled as its vector
/// is found.
struct Building {
    doc: HeldDoc,
    found: Vec<Found>,
}

impl Building {
    /// The document with the chunks whose vector was found and points
    /// somewhere, in order; the others' hashes are what it waits for.
    fn finish(self, filed: Vec<crate::FiledChunk>, dim: usize) -> Result<HeldDoc> {
        let Building { mut doc, found } = self;
        let mut kept = 0;
        for (i, (crate::FiledChunk { chunk, hash }, found)) in
            filed.into_iter().zip(found).enumerate()
        {
            match found {
                Found::Not => doc.missing.push(hash),
                Found::Zero => {}
                Found::Unit => {
                    doc.vectors.copy_within(i * dim..(i + 1) * dim, kept * dim);
                    kept += 1;
                    doc.hashes.push(hash);
                    doc.chunks.push(HeldChunk {
                        start: i64::try_from(chunk.start)?,
                        end: i64::try_from(chunk.end)?,
                        first_section: i64::try_from(chunk.first_section)?,
                        last_section: i64::try_from(chunk.last_section)?,
                    });
                }
            }
        }
        doc.vectors.truncate(kept * dim);
        doc.vectors.shrink_to_fit();
        Ok(doc)
    }
}

/// How many rows one page of [`scan_vectors`] reads.
pub const SCAN_PAGE: i64 = 4_096;

/// One page of a model's vectors after a hash, in key order: a range of
/// the primary key, read leaf by leaf.
pub const SCAN_SQL: &str = "SELECT hash, vector FROM vectors WHERE model = ? AND hash > ? \
                             ORDER BY hash LIMIT ?";

/// Every vector the cache holds under `model`, handed to `take` in key
/// order a page at a time: one pass over the table's leaves, in the order
/// they lie in the file.
async fn scan_vectors(
    cache: &SqlitePool,
    model: &str,
    take: &mut impl FnMut(&str, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut after = String::new();
    loop {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(SCAN_SQL)
            .bind(model)
            .bind(&after)
            .bind(SCAN_PAGE)
            .fetch_all(cache)
            .await
            .context("read the cached vectors")?;
        let Some((last, _)) = rows.last() else {
            return Ok(());
        };
        after = last.clone();
        for (hash, blob) in &rows {
            take(hash, blob)?;
        }
    }
}

/// The vectors of `hashes` the cache holds under `model`, handed to
/// `take`.
async fn vectors_by_key(
    cache: &SqlitePool,
    model: &str,
    hashes: &[&str],
    take: &mut impl FnMut(&str, &[u8]) -> Result<()>,
) -> Result<()> {
    for batch in hashes.chunks(BATCH) {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT hash, vector FROM vectors WHERE model = ? \
               AND hash IN (SELECT value FROM json_each(?))",
        )
        .bind(model)
        .bind(serde_json::to_string(batch)?)
        .fetch_all(cache)
        .await
        .context("read the cached vectors")?;
        for (hash, blob) in &rows {
            take(hash, blob)?;
        }
    }
    Ok(())
}

/// `(doc_id, markdown_uuid, source_id, content_hash)`.
type DocRow = (i64, String, String, String);

/// Scales `v` to unit length; false for a vector of zeros, which no query
/// is near.
fn unit(v: &mut [f32]) -> bool {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return false;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    true
}

/// A page being matched: each term's best piece score so far, and which
/// of the terms each piece holds, a bit per term.
#[derive(Clone)]
struct Page {
    markdown_uuid: String,
    best: Vec<f64>,
    holds: HashMap<i64, (Option<String>, u64)>,
}

/// A document a words search found, at its best message.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageHit {
    pub markdown_uuid: String,
    pub score: f64,
    /// The item the message is, `None` for a piece that is no item's.
    pub uuid: Option<String>,
}

/// What a search's filters keep: the documents they keep whole, and the
/// messages they keep of the rest, by uuid.
pub struct KeptRows<'a> {
    pub whole: Vec<&'a str>,
    pub messages: Vec<&'a str>,
}

/// [`KeptRows`] as each words statement binds it: whether there is a
/// filter, then the messages and the documents it keeps, each a JSON array.
type Bound = (bool, String, String);

fn bound(kept: Option<&KeptRows<'_>>) -> Result<Bound> {
    Ok(match kept {
        Some(k) => (
            true,
            serde_json::to_string(&k.messages)?,
            serde_json::to_string(&k.whole)?,
        ),
        None => (false, "[]".to_string(), "[]".to_string()),
    })
}

/// The condition on a `hit` row that keeps it, taking a [`Bound`]: no
/// filter, its message kept, or its document kept whole.
const KEEPS: &str = "(NOT ? OR hit.uuid IN (SELECT value FROM json_each(?)) \
     OR hit.markdown_uuid IN (SELECT value FROM json_each(?)))";

impl Reader {
    /// The documents whose best message `kept` keeps holds `query`, an FTS5
    /// query, of `sources` (all when `None`), best first by that message's
    /// BM25, at most `limit`, each with that message. Each document's best
    /// is taken before the cut (SQLite gives a bare column of the row
    /// `MAX` chose), so one long chat cannot fill it.
    pub async fn words(
        &self,
        query: &str,
        sources: Option<&[String]>,
        kept: Option<&KeptRows<'_>>,
        limit: usize,
    ) -> Result<Vec<MessageHit>> {
        let (filtered, messages, whole) = bound(kept)?;
        // Audited: `KEEPS` is a literal.
        let sql = format!(
            "WITH m AS MATERIALIZED ( \
                 SELECT rowid AS section_id, -bm25(search_fts, 2.0, 1.0) AS score \
                   FROM search_fts WHERE search_fts MATCH ?), \
             hit AS MATERIALIZED ( \
                 SELECT d.markdown_uuid, s.uuid, m.score \
                   FROM m JOIN search_sections s ON s.section_id = m.section_id \
                   JOIN search_documents d ON d.doc_id = s.doc_id \
                  WHERE NOT ? OR d.source_id IN (SELECT value FROM json_each(?))) \
             SELECT markdown_uuid, uuid, MAX(score) AS score FROM hit WHERE {KEEPS} \
              GROUP BY markdown_uuid ORDER BY score DESC, markdown_uuid LIMIT ?"
        );
        let rows: Vec<(String, Option<String>, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(query)
            .bind(sources.is_some())
            .bind(serde_json::to_string(&sources.unwrap_or_default())?)
            .bind(filtered)
            .bind(messages)
            .bind(whole)
            .bind(i64::try_from(limit)?)
            .fetch_all(&self.index)
            .await
            .with_context(|| format!("match {query:?} in the search index"))?;
        Ok(rows
            .into_iter()
            .map(|(markdown_uuid, uuid, score)| MessageHit {
                markdown_uuid,
                score,
                uuid,
            })
            .collect())
    }

    /// Pages (documents that are no conversation) holding every one of
    /// `terms`, each an FTS5 query, somewhere among the pieces `kept`
    /// keeps, of `sources` (all when `None`): a page's pieces are parts of
    /// one thing, where a conversation's messages are each said on their
    /// own. Best first by the sum of each term's best piece's BM25, at most
    /// `limit`, each landing on the piece holding the most of the terms, an
    /// item before the text above them.
    pub async fn page_words(
        &self,
        terms: &[String],
        sources: Option<&[String]>,
        kept: Option<&KeptRows<'_>>,
        limit: usize,
    ) -> Result<Vec<MessageHit>> {
        let kept = bound(kept)?;
        let mut pages: HashMap<i64, Page> = HashMap::new();
        for (i, term) in terms.iter().enumerate() {
            let candidates: Vec<i64> = pages.keys().copied().collect();
            let rows = self
                .term_pieces(
                    term,
                    sources,
                    &kept,
                    (i > 0).then_some(candidates.as_slice()),
                )
                .await?;
            let mut next: HashMap<i64, Page> = HashMap::new();
            for (doc_id, markdown_uuid, seq, uuid, score) in rows {
                let page = match pages.get(&doc_id) {
                    Some(held) if i > 0 => next.entry(doc_id).or_insert_with(|| held.clone()),
                    None if i == 0 => next.entry(doc_id).or_insert_with(|| Page {
                        markdown_uuid,
                        best: Vec::new(),
                        holds: HashMap::new(),
                    }),
                    _ => continue,
                };
                if page.best.len() == i {
                    page.best.push(score);
                } else if let Some(b) = page.best.get_mut(i) {
                    *b = b.max(score);
                }
                page.holds.entry(seq).or_insert((uuid, 0)).1 |= 1u64 << i.min(63);
            }
            pages = next;
            if pages.is_empty() {
                return Ok(Vec::new());
            }
        }
        let mut hits: Vec<MessageHit> = pages
            .into_values()
            .map(|p| {
                let landing = p
                    .holds
                    .iter()
                    .max_by_key(|(seq, (uuid, held))| {
                        (held.count_ones(), uuid.is_some(), std::cmp::Reverse(**seq))
                    })
                    .and_then(|(_, (uuid, _))| uuid.clone());
                MessageHit {
                    markdown_uuid: p.markdown_uuid,
                    score: p.best.iter().sum(),
                    uuid: landing,
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.markdown_uuid.cmp(&b.markdown_uuid))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// The pieces of pages holding `term` that `kept` keeps: `(doc_id,
    /// markdown_uuid, seq, uuid, score)`, of `within` documents when given.
    async fn term_pieces(
        &self,
        term: &str,
        sources: Option<&[String]>,
        (filtered, messages, whole): &Bound,
        within: Option<&[i64]>,
    ) -> Result<Vec<(i64, String, i64, Option<String>, f64)>> {
        // Audited: `KEEPS` is a literal.
        let sql = format!(
            "WITH m AS MATERIALIZED ( \
                 SELECT rowid AS section_id, -bm25(search_fts, 2.0, 1.0) AS score \
                   FROM search_fts WHERE search_fts MATCH ?), \
             hit AS MATERIALIZED ( \
                 SELECT d.doc_id, d.markdown_uuid, s.seq, s.uuid, m.score \
                   FROM m JOIN search_sections s ON s.section_id = m.section_id \
                   JOIN search_documents d ON d.doc_id = s.doc_id \
                  WHERE d.conversation = 0 \
                    AND (NOT ? OR d.source_id IN (SELECT value FROM json_each(?))) \
                    AND (NOT ? OR d.doc_id IN (SELECT value FROM json_each(?)))) \
             SELECT doc_id, markdown_uuid, seq, uuid, score FROM hit WHERE {KEEPS}"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(term)
            .bind(sources.is_some())
            .bind(serde_json::to_string(&sources.unwrap_or_default())?)
            .bind(within.is_some())
            .bind(serde_json::to_string(&within.unwrap_or_default())?)
            .bind(filtered)
            .bind(messages)
            .bind(whole)
            .fetch_all(&self.index)
            .await
            .with_context(|| format!("match {term:?} in the search index's pages"))
    }
}

pub(crate) async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {} to read", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, vectors: &[f32]) -> (String, Arc<HeldDoc>) {
        let chunk = HeldChunk {
            start: 0,
            end: 1,
            first_section: 0,
            last_section: 0,
        };
        let doc = HeldDoc {
            markdown_uuid: id.into(),
            source_id: "s".into(),
            sections: Vec::new(),
            chunks: vec![chunk; vectors.len() / 2],
            vectors: vectors.iter().map(|x| crate::half::from_f32(*x)).collect(),
            content_hash: String::new(),
            hashes: Vec::new(),
            missing: Vec::new(),
        };
        (id.to_string(), Arc::new(doc))
    }

    /// A document's place is the direction its chunks share, at unit
    /// length; one whose chunks cancel out has none and is left off, and
    /// so is one with no chunk embedded yet.
    #[test]
    fn a_document_is_the_mean_of_its_chunks() {
        let held = Held {
            dim: 2,
            docs: BTreeMap::from([
                doc("a", &[1.0, 0.0, 0.0, 1.0]),
                doc("b", &[0.0, 1.0]),
                doc("c", &[1.0, 0.0, -1.0, 0.0]),
                doc("d", &[]),
            ]),
        };
        let (ids, vectors) = held.document_vectors();
        assert_eq!(ids, ["a", "b"]);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((vectors[0] - h).abs() < 1e-6 && (vectors[1] - h).abs() < 1e-6);
        assert_eq!(&vectors[2..], [0.0, 1.0]);
    }
}
