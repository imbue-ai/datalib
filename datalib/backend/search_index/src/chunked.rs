//! The embedded chunks: for each document a source's `embed` has cut, the
//! rendering it cut (its content hash), and where each chunk falls with
//! its prompt's hash. A part of the inference cache's file, beside the
//! vectors those hashes key, so rebuilding the keyword index cuts nothing
//! again. Its writers are the `embed` steps, one at a time under the
//! `embedder` lock, and the `unified_grid_and_search_index` step's sweep,
//! which never runs beside one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use crate::chunk::Chunk;
use crate::InferenceCache;

pub(crate) const DDL: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS search_chunked (
        doc_id INTEGER PRIMARY KEY,
        source_id TEXT NOT NULL,
        markdown_uuid TEXT NOT NULL UNIQUE,
        content_hash TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS search_chunked_by_source ON search_chunked (source_id)",
    "CREATE TABLE IF NOT EXISTS search_chunks (
        doc_id INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        start_byte INTEGER NOT NULL,
        end_byte INTEGER NOT NULL,
        first_section INTEGER NOT NULL,
        last_section INTEGER NOT NULL,
        hash TEXT NOT NULL,
        PRIMARY KEY (doc_id, seq)
    ) WITHOUT ROWID",
];

/// How many documents one bound JSON array names.
const BATCH: usize = 5_000;

pub(crate) const OWED_SQL: &str = "SELECT d.markdown_uuid, d.content_hash, \
        c.start_byte, c.end_byte, c.first_section, c.last_section, c.hash \
   FROM search_chunks c JOIN search_chunked d ON d.doc_id = c.doc_id \
  WHERE d.source_id = ?1 \
    AND NOT EXISTS (SELECT 1 FROM vectors v INDEXED BY vectors_keys \
                     WHERE v.model = ?2 AND v.hash = c.hash) \
  ORDER BY d.markdown_uuid, c.seq";

/// One document's chunks as an `embed` cut them from the rendering
/// `content_hash` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkedDoc {
    pub source_id: String,
    pub markdown_uuid: String,
    pub content_hash: String,
    pub chunks: Vec<FiledChunk>,
}

/// A chunk and the hash of the prompt it is embedded as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiledChunk {
    pub chunk: Chunk,
    pub hash: String,
}

impl ChunkedDoc {
    pub fn from_prompted(doc: &crate::IndexDoc, prompted: &[crate::chunk::Prompted]) -> ChunkedDoc {
        ChunkedDoc {
            source_id: doc.source_id.clone(),
            markdown_uuid: doc.markdown_uuid.clone(),
            content_hash: doc.content_hash.clone(),
            chunks: prompted
                .iter()
                .map(|p| FiledChunk {
                    chunk: p.chunk,
                    hash: p.hash.clone(),
                })
                .collect(),
        }
    }
}

impl InferenceCache {
    /// `markdown_uuid → content_hash` for every document of `source_id`
    /// that has been cut.
    pub async fn chunked(&self, source_id: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT markdown_uuid, content_hash FROM search_chunked WHERE source_id = ?",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
        .context("read the documents cut into chunks")?;
        Ok(rows.into_iter().collect())
    }

    /// Files `docs`, each replacing what was filed for it, in one
    /// transaction.
    pub async fn file_chunks(&self, docs: &[ChunkedDoc]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for doc in docs {
            let doc_id: i64 = sqlx::query_scalar(
                "INSERT INTO search_chunked (source_id, markdown_uuid, content_hash) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (markdown_uuid) DO UPDATE \
                   SET source_id = excluded.source_id, content_hash = excluded.content_hash \
                 RETURNING doc_id",
            )
            .bind(&doc.source_id)
            .bind(&doc.markdown_uuid)
            .bind(&doc.content_hash)
            .fetch_one(&mut *tx)
            .await
            .context("file a cut document")?;
            sqlx::query("DELETE FROM search_chunks WHERE doc_id = ?")
                .bind(doc_id)
                .execute(&mut *tx)
                .await?;
            let rows: Vec<serde_json::Value> = doc
                .chunks
                .iter()
                .map(|f| {
                    let c = f.chunk;
                    serde_json::json!([c.start, c.end, c.first_section, c.last_section, f.hash])
                })
                .collect();
            sqlx::query(
                "INSERT INTO search_chunks \
                 (doc_id, seq, start_byte, end_byte, first_section, last_section, hash) \
                 SELECT ?1, key, value ->> 0, value ->> 1, value ->> 2, value ->> 3, value ->> 4 \
                   FROM json_each(?2)",
            )
            .bind(doc_id)
            .bind(serde_json::Value::Array(rows).to_string())
            .execute(&mut *tx)
            .await
            .context("file a document's chunks")?;
        }
        tx.commit().await.context("commit the cut documents")
    }

    /// Forgets what was filed for these documents.
    pub async fn forget_chunked(&self, markdown_uuids: &[String]) -> Result<()> {
        for batch in markdown_uuids.chunks(BATCH) {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let uuids = serde_json::to_string(batch)?;
            sqlx::query(
                "DELETE FROM search_chunks WHERE doc_id IN (SELECT doc_id FROM search_chunked \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?)))",
            )
            .bind(&uuids)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM search_chunked WHERE markdown_uuid IN (SELECT value FROM json_each(?))",
            )
            .bind(&uuids)
            .execute(&mut *tx)
            .await?;
            tx.commit()
                .await
                .context("commit the forgotten documents")?;
        }
        Ok(())
    }

    /// Forgets the documents of every source but `sources`: one taken out
    /// of the config or out of search. Returns how many went.
    pub async fn keep_chunked_sources(&self, sources: &[String]) -> Result<usize> {
        let gone: Vec<String> = sqlx::query_scalar(
            "SELECT markdown_uuid FROM search_chunked \
              WHERE source_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(sources)?)
        .fetch_all(&self.pool)
        .await?;
        self.forget_chunked(&gone).await?;
        Ok(gone.len())
    }

    /// Each filed chunk of `source_id` that `model` has no vector for,
    /// asked of the cache's keys alone (`cache::KEYS`),
    /// with its document and the rendering it was cut from, in document
    /// order.
    pub async fn owed_chunks(
        &self,
        model: &str,
        source_id: &str,
    ) -> Result<Vec<(String, String, FiledChunk)>> {
        let rows: Vec<(String, String, i64, i64, i64, i64, String)> = sqlx::query_as(OWED_SQL)
            .bind(source_id)
            .bind(model)
            .fetch_all(&self.pool)
            .await
            .context("read the chunks owed a vector")?;
        rows.into_iter()
            .map(|(uuid, content_hash, start, end, first, last, hash)| {
                Ok((uuid, content_hash, filed(start, end, first, last, hash)?))
            })
            .collect()
    }

    /// Deletes every vector but `model`'s whose hash a filed chunk names.
    /// Returns how many went. Until any document is filed, every vector is
    /// kept: a cache this build has not filled chunks into yet would
    /// otherwise lose all its vectors.
    pub async fn sweep(&self, model: &str) -> Result<u64> {
        let any: Option<i64> = sqlx::query_scalar("SELECT 1 FROM search_chunked LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        if any.is_none() {
            let held: i64 = sqlx::query_scalar("SELECT count(*) FROM vectors")
                .fetch_one(&self.pool)
                .await?;
            if held > 0 {
                tracing::warn!(
                    vectors = held,
                    "no document is cut into chunks yet; the sweep keeps every vector"
                );
            }
            return Ok(0);
        }
        let gone = sqlx::query(SWEEP_SQL)
            .bind(model)
            .execute(&self.pool)
            .await
            .context("sweep the vectors no chunk names")?
            .rows_affected();
        Ok(gone)
    }
}

/// The vectors no filed chunk names, found by reading the keys' index
/// (`cache::KEYS`) rather than the table, whose every page holds a 3 KB
/// vector: a whole read of a 2.7 GB cache took 150 s cold. Only the rows
/// found are then read, by their key.
pub(crate) const SWEEP_SQL: &str = "\
    DELETE FROM vectors WHERE (model, hash) IN ( \
        SELECT model, hash FROM vectors INDEXED BY vectors_keys \
         WHERE model <> ?1 OR hash NOT IN (SELECT hash FROM search_chunks))";

pub(crate) fn filed(
    start: i64,
    end: i64,
    first: i64,
    last: i64,
    hash: String,
) -> Result<FiledChunk> {
    Ok(FiledChunk {
        chunk: Chunk {
            start: usize::try_from(start)?,
            end: usize::try_from(end)?,
            first_section: usize::try_from(first)?,
            last_section: usize::try_from(last)?,
        },
        hash,
    })
}

/// Copies the chunks the keyword index filed, before this build, into
/// the cache, for every document not cut here yet, so the vectors they
/// name survive the sweep and nothing is cut again. Reads the index's
/// file only; the index's next open empties its old tables. Returns how
/// many documents came over.
pub async fn carry_chunks_from_the_index(index: &Path, cache: &Path) -> Result<usize> {
    if !index.exists() {
        return Ok(0);
    }
    let pool = crate::read::connect(index).await?;
    let out = carry(&pool, cache).await;
    pool.close().await;
    out
}

async fn carry(index: &SqlitePool, cache: &Path) -> Result<usize> {
    let old: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'search_chunks'",
    )
    .fetch_optional(index)
    .await?;
    if old.is_none() {
        return Ok(0);
    }
    let cache = InferenceCache::open(cache).await?;
    let out = carry_into(index, &cache).await;
    cache.close().await;
    out
}

async fn carry_into(index: &SqlitePool, cache: &InferenceCache) -> Result<usize> {
    let docs: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT doc_id, source_id, markdown_uuid, content_hash FROM search_documents \
          ORDER BY doc_id",
    )
    .fetch_all(index)
    .await
    .context("read the index's documents")?;
    let mut sources: Vec<&str> = docs.iter().map(|d| d.1.as_str()).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut cut: HashSet<String> = HashSet::new();
    for source in sources {
        cut.extend(cache.chunked(source).await?.into_keys());
    }
    let mut carried = 0;
    for batch in docs.chunks(BATCH) {
        let ids: Vec<i64> = batch.iter().map(|d| d.0).collect();
        let rows: Vec<(i64, i64, i64, i64, i64, String)> = sqlx::query_as(
            "SELECT doc_id, start_byte, end_byte, first_section, last_section, hash \
               FROM search_chunks WHERE doc_id IN (SELECT value FROM json_each(?)) \
              ORDER BY doc_id, seq",
        )
        .bind(serde_json::to_string(&ids)?)
        .fetch_all(index)
        .await
        .context("read the index's chunks")?;
        let mut chunks_of: BTreeMap<i64, Vec<FiledChunk>> = BTreeMap::new();
        for (doc, start, end, first, last, hash) in rows {
            chunks_of
                .entry(doc)
                .or_default()
                .push(filed(start, end, first, last, hash)?);
        }
        let docs: Vec<ChunkedDoc> = batch
            .iter()
            .filter(|(_, _, uuid, _)| !cut.contains(uuid))
            .map(|(id, source, uuid, hash)| ChunkedDoc {
                source_id: source.clone(),
                markdown_uuid: uuid.clone(),
                content_hash: hash.clone(),
                chunks: chunks_of.remove(id).unwrap_or_default(),
            })
            .collect();
        cache.file_chunks(&docs).await?;
        carried += docs.len();
    }
    Ok(carried)
}

/// `markdown_uuid → content_hash` of every document cut into chunks: the
/// rendering each was cut from.
pub(crate) async fn read_cut(cache: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT markdown_uuid, content_hash FROM search_chunked")
            .fetch_all(cache)
            .await
            .context("read the cut documents")?;
    Ok(rows.into_iter().collect())
}

/// What a reader finds filed in the cache: `markdown_uuid → (content
/// hash, its chunks in order)`, of `uuids` (every document when `None`).
pub(crate) async fn read_filed(
    cache: &SqlitePool,
    uuids: Option<&[String]>,
) -> Result<BTreeMap<String, (String, Vec<FiledChunk>)>> {
    let mut out: BTreeMap<String, (String, Vec<FiledChunk>)> = BTreeMap::new();
    type Row = (
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    let all = "SELECT d.markdown_uuid, d.content_hash, \
                      c.start_byte, c.end_byte, c.first_section, c.last_section, c.hash \
                 FROM search_chunked d LEFT JOIN search_chunks c ON c.doc_id = d.doc_id \
                ORDER BY d.doc_id, c.seq";
    // From the asked uuids into the unique index: `NOT ? OR … IN (…)`
    // reads every cut document whatever is asked.
    let asked = "SELECT d.markdown_uuid, d.content_hash, \
                      c.start_byte, c.end_byte, c.first_section, c.last_section, c.hash \
                 FROM json_each(?) j \
                 JOIN search_chunked d ON d.markdown_uuid = j.value \
                 LEFT JOIN search_chunks c ON c.doc_id = d.doc_id \
                ORDER BY d.doc_id, c.seq";
    let mut add = |rows: Vec<Row>| -> Result<()> {
        for (uuid, content_hash, start, end, first, last, hash) in rows {
            let entry = out
                .entry(uuid)
                .or_insert_with(|| (content_hash, Vec::new()));
            if let (Some(s), Some(e), Some(f), Some(l), Some(h)) = (start, end, first, last, hash) {
                entry.1.push(filed(s, e, f, l, h)?);
            }
        }
        Ok(())
    };
    match uuids {
        None => add(sqlx::query_as(all)
            .fetch_all(cache)
            .await
            .context("read the filed chunks")?)?,
        Some(uuids) => {
            for batch in uuids.chunks(BATCH) {
                add(sqlx::query_as(asked)
                    .bind(serde_json::to_string(batch)?)
                    .fetch_all(cache)
                    .await
                    .context("read the filed chunks")?)?;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{chunk_hash, four_bytes_a_token};
    use crate::{IndexDoc, IndexSection};

    const MODEL: &str = "gemma";

    fn doc(source: &str, uuid: &str, hash: &str, sections: &[(Option<&str>, &str)]) -> IndexDoc {
        IndexDoc {
            source_id: source.into(),
            markdown_uuid: uuid.into(),
            content_hash: hash.into(),
            title: "Planning".into(),
            head: None,
            sections: sections
                .iter()
                .map(|(uuid, text)| IndexSection {
                    uuid: uuid.map(str::to_string),
                    text: text.to_string(),
                    lead: None,
                })
                .collect(),
        }
    }

    fn thread(hash: &str) -> IndexDoc {
        doc(
            "mail",
            "d1",
            hash,
            &[
                (None, "# Planning\n"),
                (Some("m1"), "lunch on friday\n"),
                (Some("m2"), "dinner on saturday\n"),
            ],
        )
    }

    fn cut(doc: &IndexDoc) -> ChunkedDoc {
        ChunkedDoc::from_prompted(doc, &doc.prompted(&four_bytes_a_token).unwrap())
    }

    async fn open() -> (tempfile::TempDir, InferenceCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = InferenceCache::open(&dir.path().join("cache.sqlite"))
            .await
            .unwrap();
        (dir, cache)
    }

    async fn filed(cache: &InferenceCache) -> BTreeMap<String, (String, Vec<FiledChunk>)> {
        read_filed(&cache.pool, None).await.unwrap()
    }

    fn hashes(doc: &ChunkedDoc) -> Vec<String> {
        doc.chunks.iter().map(|c| c.hash.clone()).collect()
    }

    /// A filed document's chunks are byte spans of it, read back as they
    /// were cut; filing it again from a new rendering replaces them.
    #[tokio::test]
    async fn a_document_is_filed_as_cut_and_refiling_replaces_it() {
        let (_dir, cache) = open().await;
        let first = cut(&thread("h1"));
        cache
            .file_chunks(std::slice::from_ref(&first))
            .await
            .unwrap();
        assert_eq!(
            filed(&cache).await,
            BTreeMap::from([("d1".into(), ("h1".into(), first.chunks.clone()))])
        );
        let changed = cut(&doc(
            "mail",
            "d1",
            "h2",
            &[(Some("m3"), "dinner on sunday")],
        ));
        cache
            .file_chunks(std::slice::from_ref(&changed))
            .await
            .unwrap();
        assert_eq!(
            cache.chunked("mail").await.unwrap(),
            BTreeMap::from([("d1".to_string(), "h2".to_string())])
        );
        assert_eq!(
            filed(&cache).await["d1"].1,
            [FiledChunk {
                chunk: Chunk {
                    start: 0,
                    end: 16,
                    first_section: 0,
                    last_section: 0
                },
                hash: chunk_hash("Planning", "dinner on sunday"),
            }]
        );
    }

    /// A document with no text is filed with no chunks, so it is known to
    /// be cut rather than owed.
    #[tokio::test]
    async fn a_document_with_no_text_is_filed_with_no_chunks() {
        let (_dir, cache) = open().await;
        cache
            .file_chunks(&[cut(&doc("mail", "d2", "h1", &[]))])
            .await
            .unwrap();
        assert_eq!(
            filed(&cache).await,
            BTreeMap::from([("d2".into(), ("h1".into(), Vec::new()))])
        );
    }

    /// A thread that grows by a message keeps the hashes of the chunks
    /// before it, so their cached vectors are found again.
    #[tokio::test]
    async fn a_grown_thread_keeps_its_earlier_chunks() {
        let message = |i: usize| format!("message {i}: {}\n", "words ".repeat(100));
        let sections: Vec<(Option<String>, String)> = (0..12)
            .map(|i| (Some(format!("m{i}")), message(i)))
            .collect();
        let as_doc = |n: usize, hash: &str| {
            let refs: Vec<(Option<&str>, &str)> = sections[..n]
                .iter()
                .map(|(u, t)| (u.as_deref(), t.as_str()))
                .collect();
            cut(&doc("mail", "d1", hash, &refs))
        };
        let (before, after) = (hashes(&as_doc(11, "h1")), hashes(&as_doc(12, "h2")));
        assert!(before.len() >= 2, "{before:?}");
        assert_eq!(before[..before.len() - 1], after[..before.len() - 1]);
        assert_ne!(before.last(), after.get(before.len() - 1));
    }

    /// The chunks owed a vector are the filed ones the model has none for,
    /// each with the rendering it was cut from.
    #[tokio::test]
    async fn the_chunks_owed_are_the_filed_ones_with_no_vector() {
        let (_dir, cache) = open().await;
        let long = doc(
            "mail",
            "d1",
            "h1",
            &[(Some("m1"), &"one ".repeat(1200)), (Some("m2"), "two\n")],
        );
        let filed_doc = cut(&long);
        assert!(filed_doc.chunks.len() >= 2, "{:?}", filed_doc.chunks);
        cache
            .file_chunks(std::slice::from_ref(&filed_doc))
            .await
            .unwrap();
        cache
            .put(MODEL, &[(filed_doc.chunks[0].hash.clone(), vec![1.0])])
            .await
            .unwrap();
        let owed = cache.owed_chunks(MODEL, "mail").await.unwrap();
        let expected: Vec<(String, String, FiledChunk)> = filed_doc.chunks[1..]
            .iter()
            .map(|c| ("d1".to_string(), "h1".to_string(), c.clone()))
            .collect();
        assert_eq!(owed, expected);
        assert!(cache.owed_chunks(MODEL, "chat").await.unwrap().is_empty());
        assert_eq!(
            cache
                .owed_chunks("another-model", "mail")
                .await
                .unwrap()
                .len(),
            filed_doc.chunks.len()
        );
    }

    /// A sweep keeps the model's vectors a filed chunk names and nothing
    /// else; a source no longer searched takes its chunks with it.
    #[tokio::test]
    async fn a_sweep_keeps_the_models_named_vectors_and_nothing_else() {
        let (_dir, cache) = open().await;
        let mail = cut(&thread("h1"));
        let chat = cut(&doc("chat", "c1", "h9", &[(Some("x"), "hello there")]));
        cache
            .file_chunks(&[mail.clone(), chat.clone()])
            .await
            .unwrap();
        let named = mail.chunks[0].hash.clone();
        let chats = chat.chunks[0].hash.clone();
        cache
            .put(
                MODEL,
                &[
                    (named.clone(), vec![1.0]),
                    (chats.clone(), vec![2.0]),
                    ("stale".into(), vec![3.0]),
                ],
            )
            .await
            .unwrap();
        cache
            .put("old", &[(named.clone(), vec![4.0])])
            .await
            .unwrap();
        assert_eq!(
            cache
                .keep_chunked_sources(&["mail".to_string()])
                .await
                .unwrap(),
            1
        );
        assert!(cache.chunked("chat").await.unwrap().is_empty());
        assert_eq!(cache.sweep(MODEL).await.unwrap(), 3);
        assert!(cache
            .missing(MODEL, std::slice::from_ref(&named))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            cache
                .missing(MODEL, std::slice::from_ref(&chats))
                .await
                .unwrap(),
            [chats]
        );
        assert_eq!(
            cache
                .missing("old", std::slice::from_ref(&named))
                .await
                .unwrap(),
            [named]
        );
    }

    /// Before any document is filed, a sweep deletes nothing: a cache this
    /// build has not filled chunks into yet would lose every vector.
    #[tokio::test]
    async fn a_sweep_before_any_chunk_is_filed_keeps_every_vector() {
        let (_dir, cache) = open().await;
        cache.put(MODEL, &[("a".into(), vec![1.0])]).await.unwrap();
        assert_eq!(cache.sweep(MODEL).await.unwrap(), 0);
        assert!(cache
            .missing(MODEL, &["a".into()])
            .await
            .unwrap()
            .is_empty());
    }

    /// The chunk tables are a part of the cache's file: a cache from before
    /// them, and a change to their shape, keep every vector.
    #[tokio::test]
    async fn the_chunk_tables_never_cost_the_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = InferenceCache::open(&path).await.unwrap();
        cache.put(MODEL, &[("a".into(), vec![1.0])]).await.unwrap();
        cache.file_chunks(&[cut(&thread("h1"))]).await.unwrap();
        // A cache an older build wrote: no chunk tables, no part key.
        for sql in [
            "DROP TABLE search_chunks",
            "DROP TABLE search_chunked",
            "DELETE FROM _datalib_meta WHERE key = 'search_chunks_schema_hash'",
        ] {
            sqlx::query(sql).execute(&cache.pool).await.unwrap();
        }
        cache.close().await;
        let cache = InferenceCache::open(&path).await.unwrap();
        assert!(cache
            .missing(MODEL, &["a".into()])
            .await
            .unwrap()
            .is_empty());
        assert!(cache.chunked("mail").await.unwrap().is_empty());
        cache.file_chunks(&[cut(&thread("h1"))]).await.unwrap();
        sqlx::query(
            "UPDATE _datalib_meta SET value = 'an older shape' \
              WHERE key = 'search_chunks_schema_hash'",
        )
        .execute(&cache.pool)
        .await
        .unwrap();
        cache.close().await;
        let cache = InferenceCache::open(&path).await.unwrap();
        assert!(cache.chunked("mail").await.unwrap().is_empty(), "emptied");
        assert!(cache
            .missing(MODEL, &["a".into()])
            .await
            .unwrap()
            .is_empty());
    }

    /// The keyword index's file as a build before this one left it: its
    /// documents and the chunks it cut of them.
    async fn old_index(path: &Path, docs: &[ChunkedDoc]) {
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE search_documents (doc_id INTEGER PRIMARY KEY, source_id TEXT NOT NULL, \
             markdown_uuid TEXT NOT NULL UNIQUE, content_hash TEXT NOT NULL, \
             conversation INTEGER NOT NULL)",
            "CREATE TABLE search_chunks (doc_id INTEGER NOT NULL, seq INTEGER NOT NULL, \
             start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL, \
             first_section INTEGER NOT NULL, last_section INTEGER NOT NULL, \
             hash TEXT NOT NULL, PRIMARY KEY (doc_id, seq)) WITHOUT ROWID",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        for doc in docs {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO search_documents (source_id, markdown_uuid, content_hash, \
                 conversation) VALUES (?, ?, ?, 1) RETURNING doc_id",
            )
            .bind(&doc.source_id)
            .bind(&doc.markdown_uuid)
            .bind(&doc.content_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
            for (seq, f) in doc.chunks.iter().enumerate() {
                sqlx::query("INSERT INTO search_chunks VALUES (?, ?, ?, ?, ?, ?, ?)")
                    .bind(id)
                    .bind(seq as i64)
                    .bind(f.chunk.start as i64)
                    .bind(f.chunk.end as i64)
                    .bind(f.chunk.first_section as i64)
                    .bind(f.chunk.last_section as i64)
                    .bind(&f.hash)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        pool.close().await;
    }

    /// The chunks an earlier build filed in the keyword index come over
    /// as they were, once, and never over a document already cut here:
    /// the sweep then keeps the vectors they name.
    #[tokio::test]
    async fn the_index_s_chunks_carry_over_once() {
        let dir = tempfile::tempdir().unwrap();
        let (index, cache_path) = (
            dir.path().join("db.sqlite"),
            dir.path().join("cache.sqlite"),
        );
        let mail = cut(&thread("h1"));
        let chat = cut(&doc("chat", "c1", "h9", &[(Some("x"), "hello there")]));
        old_index(&index, &[mail.clone(), chat.clone()]).await;
        let cache = InferenceCache::open(&cache_path).await.unwrap();
        let newer = cut(&doc("chat", "c1", "h10", &[(Some("x"), "hello again")]));
        cache
            .file_chunks(std::slice::from_ref(&newer))
            .await
            .unwrap();
        cache
            .put(MODEL, &[(mail.chunks[0].hash.clone(), vec![1.0])])
            .await
            .unwrap();
        cache.close().await;

        assert_eq!(
            carry_chunks_from_the_index(&index, &cache_path)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            carry_chunks_from_the_index(&index, &cache_path)
                .await
                .unwrap(),
            0
        );
        let cache = InferenceCache::open(&cache_path).await.unwrap();
        assert_eq!(
            filed(&cache).await,
            BTreeMap::from([
                ("d1".into(), ("h1".into(), mail.chunks.clone())),
                ("c1".into(), ("h10".into(), newer.chunks.clone())),
            ])
        );
        assert_eq!(cache.sweep(MODEL).await.unwrap(), 0);
        cache.close().await;
    }

    /// An index with no chunks of its own, as this build writes it, or no
    /// file at all, carries nothing and makes no cache.
    #[tokio::test]
    async fn an_index_without_chunks_carries_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.sqlite");
        let index = dir.path().join("db.sqlite");
        assert_eq!(
            carry_chunks_from_the_index(&index, &cache_path)
                .await
                .unwrap(),
            0
        );
        let built = crate::Index::open(&index).await.unwrap();
        built.close().await;
        assert_eq!(
            carry_chunks_from_the_index(&index, &cache_path)
                .await
                .unwrap(),
            0
        );
        assert!(!cache_path.exists());
    }
}
