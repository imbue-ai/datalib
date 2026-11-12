//! What a search reads: the messages one term matches, and every chunk's
//! vector with the messages it covers. Read-only, from a file a step may
//! be writing; each read is one snapshot of it.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::words::Matched;

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
    /// `None` when no step has written the index yet. A missing cache reads
    /// as one holding no vectors.
    pub async fn open(index: &Path, cache: &Path) -> Result<Option<Reader>> {
        if !index.exists() {
            return Ok(None);
        }
        let index = connect(index).await?;
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

    /// Every message `term`, an FTS5 query, matches, of the documents in
    /// `sources` (all when `None`).
    pub async fn matching(&self, term: &str, sources: Option<&[String]>) -> Result<Vec<Matched>> {
        let wanted = serde_json::to_string(&sources.unwrap_or_default())?;
        let rows: Vec<(String, i64, i64, i64, f64)> = sqlx::query_as(
            "SELECT d.markdown_uuid, s.seq, s.start_byte, s.end_byte, \
                    -bm25(sections_fts, 2.0, 1.0) \
               FROM sections_fts \
               JOIN sections s ON s.section_id = sections_fts.rowid \
               JOIN documents d ON d.doc_id = s.doc_id \
              WHERE sections_fts MATCH ?1 \
                AND (NOT ?2 OR d.source_id IN (SELECT value FROM json_each(?3)))",
        )
        .bind(term)
        .bind(sources.is_some())
        .bind(wanted)
        .fetch_all(&self.index)
        .await
        .with_context(|| format!("match {term:?} in the search index"))?;
        Ok(rows
            .into_iter()
            .map(|(markdown_uuid, seq, start, end, score)| Matched {
                markdown_uuid,
                seq,
                start,
                end,
                score,
            })
            .collect())
    }

    /// Every chunk `model` has a vector for, with its document. A chunk the
    /// cache has no vector for yet is left out: it has not been embedded.
    pub async fn held(&self, model: &str) -> Result<Held> {
        let mut tx = self.index.begin().await?;
        let docs: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT doc_id, markdown_uuid, source_id FROM documents ORDER BY doc_id",
        )
        .fetch_all(&mut *tx)
        .await
        .context("read the indexed documents")?;
        let sections: Vec<(i64, i64, Option<String>)> =
            sqlx::query_as("SELECT doc_id, seq, uuid FROM sections ORDER BY doc_id, seq")
                .fetch_all(&mut *tx)
                .await
                .context("read the indexed sections")?;
        let chunks: Vec<(i64, i64, i64, i64, i64, String)> = sqlx::query_as(
            "SELECT doc_id, start_byte, end_byte, first_section, last_section, hash \
               FROM chunks ORDER BY doc_id, seq",
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
