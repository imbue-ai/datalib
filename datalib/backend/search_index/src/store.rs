//! The index: tables in the grid index's file (plain SQLite, WAL) that
//! every source's index steps write their own documents into, and search
//! reads beside `grid_rows`. They hold where each message and chunk falls
//! in its document, each chunk's prompt hash, and the keyword postings,
//! and never the text. The vectors are in the inference cache
//! (`cache.rs`), under those hashes. Every table's name starts with
//! [`crate::TABLE_PREFIX`]; the grid index's own tables are not ours.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;
use sqlx::SqliteConnection;

use crate::chunk::{chunk_hash, chunks};
use crate::IndexDoc;

const DDL: [&str; 7] = [
    "CREATE TABLE IF NOT EXISTS search_documents (
        doc_id INTEGER PRIMARY KEY,
        source_id TEXT NOT NULL,
        markdown_uuid TEXT NOT NULL UNIQUE,
        content_hash TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS search_documents_by_source ON search_documents (source_id)",
    "CREATE TABLE IF NOT EXISTS search_sections (
        section_id INTEGER PRIMARY KEY,
        doc_id INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        uuid TEXT NULL,
        start_byte INTEGER NOT NULL,
        end_byte INTEGER NOT NULL,
        UNIQUE (doc_id, seq)
    )",
    // One row per section, rowid = section_id. Contentless: the postings
    // and no copy of the text (fts_facts_test).
    "CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
        title, body,
        content='', contentless_delete=1, tokenize='porter unicode61'
    )",
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
    "CREATE INDEX IF NOT EXISTS search_chunks_by_hash ON search_chunks (hash)",
    // Raised by every write, so a reader holding what it read can tell
    // whether the index moved without stat-ing a file the grid also writes.
    "CREATE TABLE IF NOT EXISTS search_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        generation INTEGER NOT NULL
    )",
];

/// How long a write waits for the file. The grid index's step holds it
/// for one source's load at a time: seconds when a sync changes a few
/// thousand documents, many minutes when it loads a large source whole.
const WAIT_FOR_THE_GRID: Duration = Duration::from_secs(60 * 60);

/// Documents removed in one transaction, so a large removal never holds
/// the file the grid index's step is waiting on.
const REMOVE_BATCH: usize = 500;

#[cfg(test)]
fn schema_hash() -> String {
    datalib_store_meta::schema_hash(DDL)
}

pub struct Index {
    pool: SqlitePool,
}

/// One chunk of an indexed document: its bytes in the document, and the
/// hash of the prompt it is embedded as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpan {
    pub markdown_uuid: String,
    pub start: usize,
    pub end: usize,
    pub hash: String,
}

impl Index {
    /// Opens the index in the grid index's file at `path`, creating its
    /// tables, or emptying them when they were written in another shape:
    /// every row is derived from the render stores, so the steps index
    /// everything again. The grid's own tables are left alone.
    pub async fn open(path: &Path) -> Result<Index> {
        let pool = crate::file::open(
            path,
            &DDL,
            crate::file::Owns::Part(StoreKind::SearchIndex),
            WAIT_FOR_THE_GRID,
        )
        .await
        .with_context(|| format!("open the search index in {}", path.display()))?;
        let started = sqlx::query(
            "INSERT INTO search_state (id, generation) VALUES (1, 0) ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await;
        if let Err(e) = started {
            pool.close().await;
            return Err(e).context("start the search index's generation");
        }
        Ok(Index { pool })
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// `markdown_uuid → content_hash` for every document of `source_id`.
    pub async fn indexed(&self, source_id: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT markdown_uuid, content_hash FROM search_documents WHERE source_id = ?",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
        .context("read the indexed documents")?;
        Ok(rows.into_iter().collect())
    }

    /// Indexes `docs` in one transaction. A document already held with the
    /// same hash is left alone; a changed one has its sections and chunks
    /// replaced.
    pub async fn put(&self, docs: &[IndexDoc]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut changed = false;
        for doc in docs {
            changed |= put_one(&mut tx, doc)
                .await
                .with_context(|| format!("index document {}", doc.markdown_uuid))?;
        }
        if changed {
            moved(&mut tx).await?;
        }
        tx.commit().await.context("commit the indexed documents")
    }

    pub async fn remove(&self, markdown_uuids: &[String]) -> Result<()> {
        for batch in markdown_uuids.chunks(REMOVE_BATCH) {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let ids: Vec<i64> = sqlx::query_scalar(
                "SELECT doc_id FROM search_documents \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&mut *tx)
            .await?;
            clear(&mut tx, &ids).await?;
            sqlx::query(
                "DELETE FROM search_documents WHERE doc_id IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(&ids)?)
            .execute(&mut *tx)
            .await?;
            moved(&mut tx).await?;
            tx.commit().await.context("commit the removed documents")?;
        }
        Ok(())
    }

    /// Removes every document whose source is not one of `sources`: what
    /// the aggregator does with a source taken out of the config. Returns
    /// how many went.
    pub async fn keep_sources(&self, sources: &[String]) -> Result<usize> {
        let gone: Vec<String> = sqlx::query_scalar(
            "SELECT markdown_uuid FROM search_documents \
              WHERE source_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(sources)?)
        .fetch_all(&self.pool)
        .await?;
        if !gone.is_empty() {
            self.remove(&gone).await?;
        }
        Ok(gone.len())
    }

    /// Every chunk hash the index names, each once: what the inference
    /// cache keeps on a sweep.
    pub async fn all_hashes(&self) -> Result<std::collections::HashSet<String>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT DISTINCT hash FROM search_chunks")
            .fetch_all(&self.pool)
            .await
            .context("read every chunk hash")?;
        Ok(rows.into_iter().collect())
    }

    /// Every chunk of `source_id`'s documents with where it lies, in
    /// document order: what the embed step cuts each chunk's text by.
    pub async fn chunk_spans(&self, source_id: &str) -> Result<Vec<ChunkSpan>> {
        let rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
            "SELECT d.markdown_uuid, c.start_byte, c.end_byte, c.hash FROM search_chunks c \
               JOIN search_documents d ON d.doc_id = c.doc_id \
              WHERE d.source_id = ? ORDER BY d.markdown_uuid, c.seq",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
        .context("read the indexed chunks")?;
        rows.into_iter()
            .map(|(markdown_uuid, start, end, hash)| {
                Ok(ChunkSpan {
                    markdown_uuid,
                    start: usize::try_from(start)?,
                    end: usize::try_from(end)?,
                    hash,
                })
            })
            .collect()
    }

    /// Every chunk of `source_id`'s documents: `(markdown_uuid, seq, prompt
    /// hash)`, in document order. What the embed step asks the cache about.
    pub async fn chunks(&self, source_id: &str) -> Result<Vec<(String, i64, String)>> {
        sqlx::query_as(
            "SELECT d.markdown_uuid, c.seq, c.hash FROM search_chunks c \
               JOIN search_documents d ON d.doc_id = c.doc_id \
              WHERE d.source_id = ? ORDER BY d.markdown_uuid, c.seq",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
        .context("read the indexed chunks")
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// `false` when the document was already held as it is.
async fn put_one(conn: &mut SqliteConnection, doc: &IndexDoc) -> Result<bool> {
    let held: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT doc_id, source_id, content_hash FROM search_documents WHERE markdown_uuid = ?",
    )
    .bind(&doc.markdown_uuid)
    .fetch_optional(&mut *conn)
    .await?;
    let doc_id = match held {
        Some((_, source, hash)) if source == doc.source_id && hash == doc.content_hash => {
            return Ok(false);
        }
        Some((doc_id, _, _)) => {
            clear(conn, &[doc_id]).await?;
            sqlx::query(
                "UPDATE search_documents SET source_id = ?, content_hash = ? WHERE doc_id = ?",
            )
            .bind(&doc.source_id)
            .bind(&doc.content_hash)
            .bind(doc_id)
            .execute(&mut *conn)
            .await?;
            doc_id
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO search_documents (source_id, markdown_uuid, content_hash) \
                 VALUES (?, ?, ?) RETURNING doc_id",
            )
            .bind(&doc.source_id)
            .bind(&doc.markdown_uuid)
            .bind(&doc.content_hash)
            .fetch_one(&mut *conn)
            .await?
        }
    };

    let sections = doc.sections_or_one();
    let mut at = 0i64;
    for (seq, section) in sections.iter().enumerate() {
        let end = at + section.text.len() as i64;
        let section_id: i64 = sqlx::query_scalar(
            "INSERT INTO search_sections (doc_id, seq, uuid, start_byte, end_byte) \
             VALUES (?, ?, ?, ?, ?) RETURNING section_id",
        )
        .bind(doc_id)
        .bind(seq as i64)
        .bind(&section.uuid)
        .bind(at)
        .bind(end)
        .fetch_one(&mut *conn)
        .await?;
        let title = if seq == 0 { doc.title.as_str() } else { "" };
        sqlx::query("INSERT INTO search_fts (rowid, title, body) VALUES (?, ?, ?)")
            .bind(section_id)
            .bind(title)
            .bind(&section.text)
            .execute(&mut *conn)
            .await?;
        at = end;
    }
    let texts: Vec<&str> = sections.iter().map(|s| s.text.as_str()).collect();
    let whole = texts.concat();
    for (seq, c) in chunks(&texts).iter().enumerate() {
        sqlx::query(
            "INSERT INTO search_chunks \
             (doc_id, seq, start_byte, end_byte, first_section, last_section, hash) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(doc_id)
        .bind(seq as i64)
        .bind(c.start as i64)
        .bind(c.end as i64)
        .bind(c.first_section as i64)
        .bind(c.last_section as i64)
        .bind(chunk_hash(&doc.title, &whole[c.start..c.end]))
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
}

async fn moved(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query("UPDATE search_state SET generation = generation + 1 WHERE id = 1")
        .execute(conn)
        .await
        .context("raise the search index's generation")?;
    Ok(())
}

/// Everything the index holds for these documents but their `search_documents` rows.
async fn clear(conn: &mut SqliteConnection, doc_ids: &[i64]) -> Result<()> {
    let ids = serde_json::to_string(doc_ids)?;
    for sql in [
        "DELETE FROM search_fts WHERE rowid IN \
         (SELECT section_id FROM search_sections WHERE doc_id IN (SELECT value FROM json_each(?)))",
        "DELETE FROM search_sections WHERE doc_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM search_chunks WHERE doc_id IN (SELECT value FROM json_each(?))",
    ] {
        sqlx::query(sql).bind(&ids).execute(&mut *conn).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
