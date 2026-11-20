//! What a search reads: the messages one term matches, and every chunk's
//! vector with the messages it covers. Read-only, from files a step may
//! be writing; each read is one snapshot of each. A document's chunks
//! count only when they were cut from the rendering the keyword index
//! holds: their sections are numbered in it.

use std::collections::{BTreeMap, HashMap, HashSet};
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
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT markdown_uuid, content_hash FROM search_documents WHERE source_id = ?",
        )
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
        let held: Vec<(String, String)> = sqlx::query_as(
            "SELECT markdown_uuid, content_hash FROM search_documents \
              WHERE ?1 IS NULL OR source_id = ?1",
        )
        .bind(source_id)
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
        let docs: Vec<DocRow> = sqlx::query_as(
            "SELECT doc_id, markdown_uuid, source_id, content_hash FROM search_documents \
              ORDER BY doc_id",
        )
        .fetch_all(&mut *tx)
        .await
        .context("read the indexed documents")?;
        let sections: Vec<(i64, i64, Option<String>)> =
            sqlx::query_as("SELECT doc_id, seq, uuid FROM search_sections ORDER BY doc_id, seq")
                .fetch_all(&mut *tx)
                .await
                .context("read the indexed sections")?;
        tx.commit().await?;
        let filed = match &self.cache {
            Some(cache) => crate::chunked::read_filed(cache, None).await?,
            None => BTreeMap::new(),
        };
        let vectors = match &self.cache {
            Some(cache) => {
                let mut hashes: Vec<&str> = filed
                    .values()
                    .flat_map(|(_, chunks)| chunks.iter().map(|c| c.hash.as_str()))
                    .collect();
                hashes.sort_unstable();
                hashes.dedup();
                cached(cache, model, &hashes).await?
            }
            None => HashMap::new(),
        };
        assemble(docs, sections, filed, &vectors)
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

type DocRow = (i64, String, String, String);
type SectionRow = (i64, i64, Option<String>);
type Filed = BTreeMap<String, (String, Vec<crate::FiledChunk>)>;

fn assemble(
    docs: Vec<DocRow>,
    sections: Vec<SectionRow>,
    mut filed: Filed,
    vectors: &HashMap<String, Vec<u8>>,
) -> Result<Held> {
    let mut held = Held::default();
    let mut sections_of: HashMap<i64, Vec<Option<String>>> = HashMap::new();
    for (doc, _, uuid) in sections {
        sections_of.entry(doc).or_default().push(uuid);
    }
    for (doc_id, markdown_uuid, source_id, content_hash) in docs {
        let chunks = match filed.remove(&markdown_uuid) {
            Some((cut_from, chunks)) if cut_from == content_hash => chunks,
            _ => Vec::new(),
        };
        let from = held.chunks.len();
        for crate::FiledChunk { chunk, hash } in chunks {
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
                start: i64::try_from(chunk.start)?,
                end: i64::try_from(chunk.end)?,
                first_section: i64::try_from(chunk.first_section)?,
                last_section: i64::try_from(chunk.last_section)?,
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
