//! What a search reads: the messages one term matches, and every chunk's
//! vector with the messages it covers. Read-only, from a file a step may
//! be writing; each read is one snapshot of it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Connection, SqliteConnection};

use crate::words::{self, Matched, WordsHit};

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

    /// The documents every one of `required` matched in some message and
    /// none of `excluded` matched, of `sources` and `among` (all when
    /// `None`), ranked by [`words::rank`]. Each term is an FTS5 query.
    ///
    /// The rarest term is read in full, and every other only within the
    /// documents still standing, each document's best score taken in
    /// SQLite: a common word matches most messages of a library, and
    /// reading each of those out took seconds a word. A word in more than
    /// [`COMMON`] of the messages is left out, unless it is the rarest
    /// term: BM25 gives it next to no weight, and a document that holds a
    /// query's rarer words holds it, while checking that costs the most.
    pub async fn words(
        &self,
        required: &[String],
        excluded: &[String],
        sources: Option<&[String]>,
        among: Option<&HashSet<String>>,
        limit: usize,
    ) -> Result<Vec<WordsHit>> {
        if required.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.index.acquire().await?;
        // One snapshot for every read, so a step's write between two terms
        // cannot move a section under them.
        let mut tx = conn.begin().await?;
        let messages: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(section_id), 0) FROM sections")
            .fetch_one(&mut *tx)
            .await?;
        // Counted only up to where a word is common: past it, how common
        // no longer matters.
        let common = (COMMON * messages as f64).ceil() as i64;
        let mut order = Vec::with_capacity(required.len());
        for term in required {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM \
                   (SELECT 1 FROM sections_fts WHERE sections_fts MATCH ? LIMIT ?)",
            )
            .bind(term)
            .bind(common + 1)
            .fetch_one(&mut *tx)
            .await
            .with_context(|| format!("count {term:?} in the search index"))?;
            if n == 0 {
                return Ok(Vec::new());
            }
            order.push((n, term));
        }
        order.sort_unstable();
        let terms: Vec<&String> = order
            .iter()
            .enumerate()
            .filter(|(k, (n, _))| *k == 0 || *n <= common)
            .map(|(_, (_, term))| *term)
            .collect();

        let mut matched: Vec<Vec<Matched>> = Vec::with_capacity(terms.len());
        let rarest = terms[0];
        let rows: Vec<TermRow> = sqlx::query_as(
            "WITH m AS MATERIALIZED ( \
                 SELECT s.doc_id, -bm25(sections_fts, 2.0, 1.0) AS score \
                   FROM sections_fts JOIN sections s ON s.section_id = sections_fts.rowid \
                  WHERE sections_fts MATCH ?1) \
             SELECT d.doc_id, d.markdown_uuid, MAX(m.score) \
               FROM m JOIN documents d ON d.doc_id = m.doc_id \
              WHERE NOT ?2 OR d.source_id IN (SELECT value FROM json_each(?3)) \
              GROUP BY d.doc_id",
        )
        .bind(rarest)
        .bind(sources.is_some())
        .bind(serde_json::to_string(&sources.unwrap_or_default())?)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("match {rarest:?} in the search index"))?;
        let rows: Vec<TermRow> = rows
            .into_iter()
            .filter(|r| among.is_none_or(|a| a.contains(&r.1)))
            .collect();
        let mut standing: HashSet<i64> = rows.iter().map(|r| r.0).collect();
        matched.push(rows.into_iter().map(matched_row).collect());
        if standing.is_empty() || (terms.len() == 1 && excluded.is_empty()) {
            return Ok(words::rank(&matched, &HashSet::new(), None, limit));
        }

        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS standing (\
                section_id INTEGER PRIMARY KEY, doc_id INTEGER, markdown_uuid TEXT)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM temp.standing")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO temp.standing \
             SELECT s.section_id, s.doc_id, d.markdown_uuid \
               FROM sections s JOIN documents d ON d.doc_id = s.doc_id \
              WHERE s.doc_id IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(&standing)?)
        .execute(&mut *tx)
        .await?;
        for (k, term) in terms.iter().enumerate().skip(1) {
            let rows = within_standing(&mut tx, term, true).await?;
            let now: HashSet<i64> = rows.iter().map(|r| r.0).collect();
            if now.is_empty() {
                return Ok(Vec::new());
            }
            matched.push(rows.into_iter().map(matched_row).collect());
            let last = k + 1 == terms.len() && excluded.is_empty();
            if now.len() < standing.len() && !last {
                sqlx::query(
                    "DELETE FROM temp.standing \
                      WHERE doc_id NOT IN (SELECT value FROM json_each(?))",
                )
                .bind(serde_json::to_string(&now)?)
                .execute(&mut *tx)
                .await?;
            }
            standing = now;
        }
        let mut gone = HashSet::new();
        for term in excluded {
            gone.extend(
                within_standing(&mut tx, term, false)
                    .await?
                    .into_iter()
                    .map(|r| r.1),
            );
        }
        tx.rollback().await?;
        Ok(words::rank(&matched, &gone, None, limit))
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

/// The share of all messages past which a word is left out of a search
/// that has a rarer one.
const COMMON: f64 = 0.05;

/// A document, its id, and its best score for a term.
type TermRow = (i64, String, f64);

/// Every standing document `term` matches, with its best score when
/// `scored`, else 0. The full-text scan drives and each match is looked up
/// among the standing messages, never the other way round: FTS5 seeks one
/// row id at a time. `bm25()` is read in a query of its own, which SQLite
/// would otherwise fold into the grouping one, where it cannot be called.
async fn within_standing(
    conn: &mut SqliteConnection,
    term: &str,
    scored: bool,
) -> Result<Vec<TermRow>> {
    let sql = if scored {
        "WITH m AS MATERIALIZED ( \
             SELECT c.doc_id, c.markdown_uuid, -bm25(sections_fts, 2.0, 1.0) AS score \
               FROM sections_fts CROSS JOIN temp.standing c \
                 ON c.section_id = sections_fts.rowid \
              WHERE sections_fts MATCH ?) \
         SELECT doc_id, markdown_uuid, MAX(score) FROM m GROUP BY doc_id"
    } else {
        "SELECT c.doc_id, c.markdown_uuid, 0.0 \
           FROM sections_fts CROSS JOIN temp.standing c ON c.section_id = sections_fts.rowid \
          WHERE sections_fts MATCH ? GROUP BY c.doc_id"
    };
    sqlx::query_as(sql)
        .bind(term)
        .fetch_all(conn)
        .await
        .with_context(|| format!("match {term:?} in the search index"))
}

fn matched_row((_, markdown_uuid, score): TermRow) -> Matched {
    Matched {
        markdown_uuid,
        score,
    }
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
