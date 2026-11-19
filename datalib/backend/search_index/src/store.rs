//! The keyword index: tables in the grid index's file (plain SQLite, WAL)
//! that the `unified_grid_and_search_index` step writes each searched
//! source's documents into, and search reads beside `grid_rows`. They
//! hold where each message falls in its document and the keyword
//! postings, and never the text. Where each chunk falls, and its vector,
//! are in the inference cache's file, which each source's `embed` writes
//! (`chunked.rs`, `cache.rs`). Every table's name starts with
//! [`crate::TABLE_PREFIX`]; the grid index's own tables are not ours.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;
use sqlx::SqliteConnection;

use crate::IndexDoc;

const DDL: [&str; 6] = [
    // `conversation`: its pieces are messages, each its own utterance, so a
    // word search needs one to hold every word; otherwise it is a page,
    // whose pieces are parts of one thing and may hold the words between
    // them.
    "CREATE TABLE IF NOT EXISTS search_documents (
        doc_id INTEGER PRIMARY KEY,
        source_id TEXT NOT NULL,
        markdown_uuid TEXT NOT NULL UNIQUE,
        content_hash TEXT NOT NULL,
        conversation INTEGER NOT NULL
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
    // Raised by every write, so a reader holding what it read can tell
    // whether the index moved without stat-ing a file the grid also writes.
    "CREATE TABLE IF NOT EXISTS search_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        generation INTEGER NOT NULL
    )",
    // The render commit and the text version a source's keyword pass last
    // finished at, so a pass whose source has not moved reads nothing.
    "CREATE TABLE IF NOT EXISTS search_sources (
        source_id TEXT PRIMARY KEY,
        read_at TEXT NOT NULL
    )",
];

/// How long a write waits for the file. The grid index's step holds it
/// for one source's load at a time: seconds when a sync changes a few
/// thousand documents, many minutes when it loads a large source whole.
const WAIT_FOR_THE_GRID: Duration = Duration::from_secs(60 * 60);

/// About how much drawn text one transaction of [`Index::put`] indexes.
/// Its FTS rows take a few hundred milliseconds a megabyte; a whole batch
/// of large documents in one held another writer of the file for over
/// half a minute.
pub const PUT_BYTES: usize = 1 << 20;

/// Documents removed in one transaction, so a large removal never holds
/// the file the grid index's step is waiting on.
const REMOVE_BATCH: usize = 500;

/// The shape of the index's tables, as its `_datalib_meta` part records it.
pub(crate) fn shape() -> String {
    datalib_store_meta::schema_hash(DDL)
}

pub struct Index {
    pool: SqlitePool,
}

impl Index {
    /// Opens the index in the grid index's file at `path`, creating its
    /// tables, or emptying them when they were written in another shape:
    /// every row is derived from the render stores, so the steps index
    /// everything again. The grid's own tables are left alone.
    pub async fn open(path: &Path) -> Result<Index> {
        let pool = crate::file::open(
            path,
            &[(&DDL, crate::file::Owns::Part(StoreKind::SearchIndex))],
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

    /// Indexes `docs`, committing after about [`PUT_BYTES`] of their text
    /// so the file's other writers wait seconds, not the whole batch. A
    /// document already held with the same hash is left alone; a changed
    /// one has its sections replaced, in one transaction.
    pub async fn put(&self, docs: &[IndexDoc]) -> Result<()> {
        let mut rest = docs;
        while !rest.is_empty() {
            let mut bytes = 0;
            let take = rest
                .iter()
                .take_while(|d| {
                    let first = bytes == 0;
                    bytes += d.sections.iter().map(|s| s.text.len()).sum::<usize>();
                    first || bytes <= PUT_BYTES
                })
                .count();
            let (now, later) = rest.split_at(take);
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let mut changed = false;
            for doc in now {
                changed |= put_one(&mut tx, doc)
                    .await
                    .with_context(|| format!("index document {}", doc.markdown_uuid))?;
            }
            if changed {
                moved(&mut tx).await?;
            }
            tx.commit().await.context("commit the indexed documents")?;
            rest = later;
        }
        Ok(())
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

    /// Removes every document whose source is not one of `sources`, and
    /// what its keyword pass last read: a source taken out of the config or
    /// out of search. Returns how many documents went.
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
        sqlx::query(
            "DELETE FROM search_sources WHERE source_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(sources)?)
        .execute(&self.pool)
        .await
        .context("forget the passes of sources no longer searched")?;
        Ok(gone.len())
    }

    /// What `source_id`'s last finished keyword pass read, as
    /// [`Index::set_read_at`] wrote it; `None` before its first.
    pub async fn read_at(&self, source_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT read_at FROM search_sources WHERE source_id = ?")
            .bind(source_id)
            .fetch_optional(&self.pool)
            .await
            .context("read where the source's keyword pass last read")
    }

    /// Records that `source_id`'s documents are in line with what it read
    /// at `read_at`; `None` forgets it, so the next pass reads it whole.
    pub async fn set_read_at(&self, source_id: &str, read_at: Option<&str>) -> Result<()> {
        let done = match read_at {
            Some(at) => {
                sqlx::query(
                    "INSERT INTO search_sources (source_id, read_at) VALUES (?1, ?2) \
                 ON CONFLICT (source_id) DO UPDATE SET read_at = excluded.read_at",
                )
                .bind(source_id)
                .bind(at)
                .execute(&self.pool)
                .await
            }
            None => {
                sqlx::query("DELETE FROM search_sources WHERE source_id = ?")
                    .bind(source_id)
                    .execute(&self.pool)
                    .await
            }
        };
        done.context("record where the source's keyword pass read")?;
        Ok(())
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
                "UPDATE search_documents SET source_id = ?, content_hash = ?, conversation = ? \
                 WHERE doc_id = ?",
            )
            .bind(&doc.source_id)
            .bind(&doc.content_hash)
            .bind(doc.is_conversation())
            .bind(doc_id)
            .execute(&mut *conn)
            .await?;
            doc_id
        }
        None => sqlx::query_scalar(
            "INSERT INTO search_documents (source_id, markdown_uuid, content_hash, conversation) \
                 VALUES (?, ?, ?, ?) RETURNING doc_id",
        )
        .bind(&doc.source_id)
        .bind(&doc.markdown_uuid)
        .bind(&doc.content_hash)
        .bind(doc.is_conversation())
        .fetch_one(&mut *conn)
        .await?,
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
    ] {
        sqlx::query(sql).bind(&ids).execute(&mut *conn).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
