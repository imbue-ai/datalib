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
use sqlx::sqlite::SqlitePool;
use sqlx::SqliteConnection;

use crate::IndexDoc;
use datalib_sql::{sql, sql_id};
use sqlx::Row;

/// The keyword index's tables: a part of the grid index's file
/// (`grid_index::Part::Keywords`), whose writer makes them, and empties
/// them alone when they move; [`Index::open`] makes any that are missing.
pub const DDL: [&str; 6] = [
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
    // and no copy of the text (fts_facts_test). Each word as written, case
    // and accents folded, so a quoted phrase is exact: another form of a
    // word is meaning's to find, not a stemmer's.
    "CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
        title, body,
        content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2'
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

pub struct Index {
    pool: SqlitePool,
}

impl Index {
    /// Opens the index in the grid index's file at `path`.
    pub async fn open(path: &Path) -> Result<Index> {
        let pool = crate::file::open(path, &DDL, WAIT_FOR_THE_GRID)
            .await
            .with_context(|| format!("open the search index in {}", path.display()))?;
        let started = sql!(
            "search_index.store.insert_initial_generation",
            "INSERT INTO search_state (id, generation) VALUES (1, 0) ON CONFLICT (id) DO NOTHING"
        )
        .query()
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
        let rows: Vec<(String, String)> = sql!(
            "search_index.store.list_documents_of_source",
            "SELECT markdown_uuid, content_hash FROM search_documents WHERE source_id = ?"
        )
        .query_as()
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
            if put_batch(&mut tx, now)
                .await
                .context("index a batch of documents")?
            {
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
            let ids: Vec<i64> = sql!(
                "search_index.store.find_doc_ids_to_remove",
                "SELECT doc_id FROM search_documents \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
            )
            .query_scalar()
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&mut *tx)
            .await?;
            clear(&mut tx, &ids).await?;
            sql!(
                "search_index.store.delete_documents_by_id",
                "DELETE FROM search_documents WHERE doc_id IN (SELECT value FROM json_each(?))"
            )
            .query()
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
        let gone: Vec<String> = sql!(
            "search_index.store.list_documents_of_dropped_sources",
            "SELECT markdown_uuid FROM search_documents \
              WHERE source_id NOT IN (SELECT value FROM json_each(?))"
        )
        .query_scalar()
        .bind(serde_json::to_string(sources)?)
        .fetch_all(&self.pool)
        .await?;
        if !gone.is_empty() {
            self.remove(&gone).await?;
        }
        sql!(
            "search_index.store.delete_passes_of_dropped_sources",
            "DELETE FROM search_sources WHERE source_id NOT IN (SELECT value FROM json_each(?))"
        )
        .query()
        .bind(serde_json::to_string(sources)?)
        .execute(&self.pool)
        .await
        .context("forget the passes of sources no longer searched")?;
        Ok(gone.len())
    }

    /// What `source_id`'s last finished keyword pass read, as
    /// [`Index::set_read_at`] wrote it; `None` before its first.
    pub async fn read_at(&self, source_id: &str) -> Result<Option<String>> {
        sql!(
            "search_index.store.read_pass_position",
            "SELECT read_at FROM search_sources WHERE source_id = ?"
        )
        .query_scalar()
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
                sql!(
                    "search_index.store.upsert_pass_position",
                    "INSERT INTO search_sources (source_id, read_at) VALUES (?1, ?2) \
                 ON CONFLICT (source_id) DO UPDATE SET read_at = excluded.read_at"
                )
                .query()
                .bind(source_id)
                .bind(at)
                .execute(&self.pool)
                .await
            }
            None => {
                sql!(
                    "search_index.store.delete_pass_position",
                    "DELETE FROM search_sources WHERE source_id = ?"
                )
                .query()
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

/// Indexes `docs` in a few statements: those already held with the same
/// source and hash are left alone, a changed one has its sections
/// replaced under the id it had, a new one gets one. `false` when every
/// one was already held as it is.
async fn put_batch(conn: &mut SqliteConnection, docs: &[IndexDoc]) -> Result<bool> {
    let held: BTreeMap<String, (i64, String, String)> = sql!(
        "search_index.store.find_documents_of_batch",
        "SELECT markdown_uuid, doc_id, source_id, content_hash FROM search_documents \
          WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    )
    .query_as::<(String, i64, String, String)>()
    .bind(serde_json::to_string(
        &docs
            .iter()
            .map(|d| d.markdown_uuid.as_str())
            .collect::<Vec<_>>(),
    )?)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(uuid, id, source, hash)| (uuid, (id, source, hash)))
    .collect();
    // The last of a document named twice is the one indexed.
    let mut last: BTreeMap<&str, &IndexDoc> = BTreeMap::new();
    for doc in docs {
        last.insert(&doc.markdown_uuid, doc);
    }
    let owed: Vec<&IndexDoc> = last
        .into_values()
        .filter(|d| {
            held.get(&d.markdown_uuid)
                .is_none_or(|(_, source, hash)| *source != d.source_id || *hash != d.content_hash)
        })
        .collect();
    if owed.is_empty() {
        return Ok(false);
    }
    let replaced: Vec<i64> = owed
        .iter()
        .filter_map(|d| held.get(&d.markdown_uuid).map(|(id, _, _)| *id))
        .collect();
    clear(conn, &replaced).await?;
    let ids: BTreeMap<String, i64> = datalib_sql::values::fetch(
        conn,
        sql_id!("search_index.store.upsert_documents"),
        "INSERT INTO search_documents (source_id, markdown_uuid, content_hash, conversation)",
        4,
        "ON CONFLICT (markdown_uuid) DO UPDATE SET source_id = excluded.source_id, \
         content_hash = excluded.content_hash, conversation = excluded.conversation \
         RETURNING markdown_uuid, doc_id",
        &owed,
        |q, d| {
            q.bind(&d.source_id)
                .bind(&d.markdown_uuid)
                .bind(&d.content_hash)
                .bind(d.is_conversation())
        },
    )
    .await?
    .iter()
    .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
    .collect::<Result<_, sqlx::Error>>()?;
    let mut sections: Vec<Placed> = Vec::new();
    for doc in &owed {
        let doc_id = *ids
            .get(&doc.markdown_uuid)
            .with_context(|| format!("no id for document {}", doc.markdown_uuid))?;
        let mut at = 0i64;
        for (seq, section) in doc.sections_or_one().iter().enumerate() {
            let end = at + section.text.len() as i64;
            sections.push(Placed {
                doc_id,
                seq: seq as i64,
                uuid: section.uuid.clone(),
                start: at,
                end,
                title: if seq == 0 {
                    doc.title.clone()
                } else {
                    String::new()
                },
                text: section.text.clone(),
            });
            at = end;
        }
    }
    let section_ids: BTreeMap<(i64, i64), i64> = datalib_sql::values::fetch(
        conn,
        sql_id!("search_index.store.insert_sections"),
        "INSERT INTO search_sections (doc_id, seq, uuid, start_byte, end_byte)",
        5,
        "RETURNING doc_id, seq, section_id",
        &sections,
        |q, s| {
            q.bind(s.doc_id)
                .bind(s.seq)
                .bind(&s.uuid)
                .bind(s.start)
                .bind(s.end)
        },
    )
    .await?
    .iter()
    .map(|r| Ok(((r.try_get(0)?, r.try_get(1)?), r.try_get(2)?)))
    .collect::<Result<_, sqlx::Error>>()?;
    let texts = sections
        .iter()
        .map(|s| {
            let id = *section_ids
                .get(&(s.doc_id, s.seq))
                .with_context(|| format!("no id for section {} of document {}", s.seq, s.doc_id))?;
            Ok((id, s))
        })
        .collect::<Result<Vec<_>>>()?;
    datalib_sql::values::fetch(
        conn,
        sql_id!("search_index.store.insert_fts_texts"),
        "INSERT INTO search_fts (rowid, title, body)",
        3,
        "",
        &texts,
        |q, (id, s)| q.bind(*id).bind(&s.title).bind(&s.text),
    )
    .await?;
    Ok(true)
}

/// One section of a document as the index places it: where it falls in
/// the document, and the words it indexes.
struct Placed {
    doc_id: i64,
    seq: i64,
    uuid: Option<String>,
    start: i64,
    end: i64,
    title: String,
    text: String,
}

async fn moved(conn: &mut SqliteConnection) -> Result<()> {
    sql!(
        "search_index.store.bump_generation",
        "UPDATE search_state SET generation = generation + 1 WHERE id = 1"
    )
    .query()
    .execute(conn)
    .await
    .context("raise the search index's generation")?;
    Ok(())
}

/// Everything the index holds for these documents but their `search_documents` rows.
async fn clear(conn: &mut SqliteConnection, doc_ids: &[i64]) -> Result<()> {
    let ids = serde_json::to_string(doc_ids)?;
    for sql in [
        sql!(
            "search_index.clear.delete_search_fts",
            "DELETE FROM search_fts WHERE rowid IN \
         (SELECT section_id FROM search_sections WHERE doc_id IN (SELECT value FROM json_each(?)))"
        ),
        sql!(
            "search_index.clear.delete_search_sections",
            "DELETE FROM search_sections WHERE doc_id IN (SELECT value FROM json_each(?))"
        ),
    ] {
        sql.query().bind(&ids).execute(&mut *conn).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
