//! What a search reads: the messages one term matches, and every chunk's
//! vector with the messages it covers. Read-only, from a file a step may
//! be writing; each read is one snapshot of it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// How many hashes go into one statement's bound JSON array.
const BATCH: usize = 10_000;

pub struct Reader {
    index: SqlitePool,
    cache: Option<SqlitePool>,
}

/// Every chunk a model embedded, ready to score: one vector per chunk, unit
/// length, laid end to end.
#[derive(Debug, Default)]
pub struct Held {
    pub dim: usize,
    pub docs: Vec<HeldDoc>,
    pub chunks: Vec<HeldChunk>,
    pub vectors: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDoc {
    pub markdown_uuid: String,
    pub source_id: String,
    /// Each section's message uuid, by `seq`.
    pub sections: Vec<Option<String>>,
    /// Its chunks' places in [`Held::chunks`].
    pub chunks: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldChunk {
    pub start: i64,
    pub end: i64,
    pub first_section: i64,
    pub last_section: i64,
}

impl Held {
    pub fn vector(&self, chunk: usize) -> &[f32] {
        &self.vectors[chunk * self.dim..(chunk + 1) * self.dim]
    }

    /// Each embedded document's place in meaning: the mean of its chunks'
    /// unit vectors, scaled to unit length, laid end to end, with its id.
    pub fn document_vectors(&self) -> (Vec<String>, Vec<f32>) {
        let mut ids = Vec::with_capacity(self.docs.len());
        let mut out = Vec::with_capacity(self.docs.len() * self.dim);
        for doc in &self.docs {
            let mut sum = vec![0f32; self.dim];
            for chunk in doc.chunks.clone() {
                for (s, x) in sum.iter_mut().zip(self.vector(chunk)) {
                    *s += x;
                }
            }
            if unit(&mut sum) {
                ids.push(doc.markdown_uuid.clone());
                out.extend(sum);
            }
        }
        (ids, out)
    }

    /// The message uuids chunk `chunk` of `doc` covers.
    pub fn covered<'a>(&self, doc: &'a HeldDoc, chunk: usize) -> impl Iterator<Item = &'a str> {
        let c = &self.chunks[chunk];
        let from = usize::try_from(c.first_section).unwrap_or(0);
        let to = usize::try_from(c.last_section + 1).unwrap_or(0);
        doc.sections
            .get(from..to.min(doc.sections.len()))
            .unwrap_or_default()
            .iter()
            .filter_map(Option::as_deref)
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

    /// How many documents the index holds, embedded or not.
    pub async fn documents(&self) -> Result<usize> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_documents")
            .fetch_one(&self.index)
            .await
            .context("count the indexed documents")?;
        Ok(usize::try_from(n)?)
    }

    /// Of `uuids`, the ones the index holds, each with whether every one of
    /// its chunks has a vector under `model`.
    pub async fn states(&self, model: &str, uuids: &[String]) -> Result<HashMap<String, bool>> {
        let mut chunks_of: HashMap<String, Vec<String>> = HashMap::new();
        for batch in uuids.chunks(BATCH) {
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT d.markdown_uuid, c.hash FROM search_documents d \
                   LEFT JOIN search_chunks c ON c.doc_id = d.doc_id \
                  WHERE d.markdown_uuid IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&self.index)
            .await
            .context("read the documents' chunks")?;
            for (uuid, hash) in rows {
                let hashes = chunks_of.entry(uuid).or_default();
                hashes.extend(hash);
            }
        }
        let all: Vec<&str> = chunks_of.values().flatten().map(String::as_str).collect();
        let have = self.vectors_held(model, &all).await?;
        Ok(chunks_of
            .into_iter()
            .map(|(uuid, hashes)| {
                let embedded = hashes.iter().all(|h| have.contains(h.as_str()));
                (uuid, embedded)
            })
            .collect())
    }

    /// How many documents the index holds, and of them how many have a
    /// vector under `model` for every chunk.
    pub async fn coverage(&self, model: &str) -> Result<(u64, u64)> {
        let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT d.doc_id, c.hash FROM search_documents d \
               LEFT JOIN search_chunks c ON c.doc_id = d.doc_id",
        )
        .fetch_all(&self.index)
        .await
        .context("read every document's chunks")?;
        let mut hashes: Vec<&str> = rows.iter().filter_map(|r| r.1.as_deref()).collect();
        hashes.sort_unstable();
        hashes.dedup();
        let have = self.vectors_held(model, &hashes).await?;
        let mut whole: HashMap<i64, bool> = HashMap::new();
        for (doc, hash) in &rows {
            let ok = hash.as_deref().is_none_or(|h| have.contains(h));
            *whole.entry(*doc).or_insert(true) &= ok;
        }
        let embedded = whole.values().filter(|ok| **ok).count();
        Ok((whole.len() as u64, embedded as u64))
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

    /// The documents whose best message holds `query`, an FTS5 query, of
    /// `sources` and `among` (all when `None`), best first by that
    /// message's BM25, at most `limit`, each with that message.
    pub async fn words(
        &self,
        query: &str,
        sources: Option<&[String]>,
        among: Option<&HashSet<String>>,
        limit: usize,
    ) -> Result<Vec<MessageHit>> {
        let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
            "WITH m AS MATERIALIZED ( \
                 SELECT rowid AS section_id, -bm25(search_fts, 2.0, 1.0) AS score \
                   FROM search_fts WHERE search_fts MATCH ?1) \
             SELECT d.markdown_uuid, s.start_byte, s.end_byte, m.score \
               FROM m JOIN search_sections s ON s.section_id = m.section_id \
               JOIN search_documents d ON d.doc_id = s.doc_id \
              WHERE (NOT ?2 OR d.source_id IN (SELECT value FROM json_each(?3))) \
                AND (NOT ?4 OR d.markdown_uuid IN (SELECT value FROM json_each(?5))) \
              ORDER BY m.score DESC LIMIT ?6",
        )
        .bind(query)
        .bind(sources.is_some())
        .bind(serde_json::to_string(&sources.unwrap_or_default())?)
        .bind(among.is_some())
        .bind(serde_json::to_string(
            &among.map(|a| a.iter().collect::<Vec<_>>()),
        )?)
        .bind(i64::try_from(limit.saturating_mul(MESSAGES_PER_DOCUMENT))?)
        .fetch_all(&self.index)
        .await
        .with_context(|| format!("match {query:?} in the search index"))?;
        Ok(best_per_document(rows, limit))
    }

    /// Every chunk `model` has a vector for, with its document. A chunk the
    /// cache has no vector for yet is left out: it has not been embedded.
    pub async fn held(&self, model: &str) -> Result<Held> {
        // Nothing embedded yet is the usual state until a sync turns
        // embedding on, and reading every section to find that out took
        // seconds.
        let any: Option<i64> = match &self.cache {
            Some(cache) => sqlx::query_scalar("SELECT 1 FROM vectors WHERE model = ? LIMIT 1")
                .bind(model)
                .fetch_optional(cache)
                .await
                .context("look for a cached vector")?,
            None => None,
        };
        if any.is_none() {
            return Ok(Held::default());
        }
        let mut tx = self.index.begin().await?;
        let docs: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT doc_id, markdown_uuid, source_id FROM search_documents ORDER BY doc_id",
        )
        .fetch_all(&mut *tx)
        .await
        .context("read the indexed documents")?;
        let sections: Vec<(i64, i64, Option<String>)> =
            sqlx::query_as("SELECT doc_id, seq, uuid FROM search_sections ORDER BY doc_id, seq")
                .fetch_all(&mut *tx)
                .await
                .context("read the indexed sections")?;
        let chunks: Vec<(i64, i64, i64, i64, i64, String)> = sqlx::query_as(
            "SELECT doc_id, start_byte, end_byte, first_section, last_section, hash \
               FROM search_chunks ORDER BY doc_id, seq",
        )
        .fetch_all(&mut *tx)
        .await
        .context("read the indexed chunks")?;
        tx.commit().await?;

        let vectors = match &self.cache {
            Some(cache) => {
                let mut hashes: Vec<&str> = chunks.iter().map(|c| c.5.as_str()).collect();
                hashes.sort_unstable();
                hashes.dedup();
                cached(cache, model, &hashes).await?
            }
            None => HashMap::new(),
        };
        assemble(docs, sections, chunks, &vectors)
    }
}

/// `hash → vector` for each of `hashes` the cache holds under `model`.
async fn cached(
    cache: &SqlitePool,
    model: &str,
    hashes: &[&str],
) -> Result<HashMap<String, Vec<u8>>> {
    let mut out = HashMap::with_capacity(hashes.len());
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
        out.extend(rows);
    }
    Ok(out)
}

type DocRow = (i64, String, String);
type SectionRow = (i64, i64, Option<String>);
type ChunkRow = (i64, i64, i64, i64, i64, String);

fn assemble(
    docs: Vec<DocRow>,
    sections: Vec<SectionRow>,
    chunks: Vec<ChunkRow>,
    vectors: &HashMap<String, Vec<u8>>,
) -> Result<Held> {
    let mut held = Held::default();
    let mut sections_of: HashMap<i64, Vec<Option<String>>> = HashMap::new();
    for (doc, _, uuid) in sections {
        sections_of.entry(doc).or_default().push(uuid);
    }
    let mut chunks_of: HashMap<i64, Vec<ChunkRow>> = HashMap::new();
    for c in chunks {
        chunks_of.entry(c.0).or_default().push(c);
    }
    for (doc_id, markdown_uuid, source_id) in docs {
        let from = held.chunks.len();
        for (_, start, end, first_section, last_section, hash) in
            chunks_of.remove(&doc_id).unwrap_or_default()
        {
            let Some(blob) = vectors.get(&hash) else {
                continue;
            };
            let (words, rest) = blob.as_chunks::<4>();
            if !rest.is_empty() || words.is_empty() {
                bail!("a cached vector of {} bytes, not whole f32s", blob.len());
            }
            if held.dim == 0 {
                held.dim = words.len();
            } else if held.dim != words.len() {
                bail!(
                    "cached vectors of {} and {} dimensions under one model",
                    held.dim,
                    words.len()
                );
            }
            let at = held.vectors.len();
            held.vectors
                .extend(words.iter().map(|w| f32::from_le_bytes(*w)));
            if !unit(&mut held.vectors[at..]) {
                held.vectors.truncate(at);
                continue;
            }
            held.chunks.push(HeldChunk {
                start,
                end,
                first_section,
                last_section,
            });
        }
        let to = held.chunks.len();
        if to > from {
            held.docs.push(HeldDoc {
                markdown_uuid,
                source_id,
                sections: sections_of.remove(&doc_id).unwrap_or_default(),
                chunks: from..to,
            });
        }
    }
    Ok(held)
}

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

/// How many messages a words search reads for each document it may
/// answer: a document's later matches are dropped, so a cut of `limit`
/// messages could hold far fewer documents.
const MESSAGES_PER_DOCUMENT: usize = 8;

/// A document a words search found, at its best message.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageHit {
    pub markdown_uuid: String,
    pub score: f64,
    pub start: i64,
    pub end: i64,
}

/// `rows`, best first, kept at each document's first, at most `limit`.
fn best_per_document(rows: Vec<(String, i64, i64, f64)>, limit: usize) -> Vec<MessageHit> {
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter(|(doc, ..)| seen.insert(doc.clone()))
        .take(limit)
        .map(|(markdown_uuid, start, end, score)| MessageHit {
            markdown_uuid,
            score,
            start,
            end,
        })
        .collect()
}

async fn connect(path: &Path) -> Result<SqlitePool> {
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

    fn doc(id: &str, chunks: std::ops::Range<usize>) -> HeldDoc {
        HeldDoc {
            markdown_uuid: id.into(),
            source_id: "s".into(),
            sections: Vec::new(),
            chunks,
        }
    }

    fn chunk() -> HeldChunk {
        HeldChunk {
            start: 0,
            end: 1,
            first_section: 0,
            last_section: 0,
        }
    }

    /// A document's place is the direction its chunks share, at unit
    /// length; one whose chunks cancel out has none and is left off.
    #[test]
    fn a_document_is_the_mean_of_its_chunks() {
        let held = Held {
            dim: 2,
            docs: vec![doc("a", 0..2), doc("b", 2..3), doc("c", 3..5)],
            chunks: vec![chunk(); 5],
            vectors: vec![1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, -1.0, 0.0],
        };
        let (ids, vectors) = held.document_vectors();
        assert_eq!(ids, ["a", "b"]);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((vectors[0] - h).abs() < 1e-6 && (vectors[1] - h).abs() < 1e-6);
        assert_eq!(&vectors[2..], [0.0, 1.0]);
    }
}
