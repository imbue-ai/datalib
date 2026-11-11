//! The index file: one plain SQLite file in WAL mode that every source's
//! index steps write their own documents into, and search reads. It holds
//! where each message and chunk falls in its document, the keyword
//! postings and the vectors, and never the text.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, SqliteConnection};

use crate::chunk::chunks;
use crate::IndexDoc;

const DDL: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS documents (
        doc_id INTEGER PRIMARY KEY,
        source_id TEXT NOT NULL,
        markdown_uuid TEXT NOT NULL UNIQUE,
        content_hash TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS documents_by_source ON documents (source_id)",
    "CREATE TABLE IF NOT EXISTS sections (
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
    "CREATE VIRTUAL TABLE IF NOT EXISTS sections_fts USING fts5(
        title, body,
        content='', contentless_delete=1, tokenize='porter unicode61'
    )",
    "CREATE TABLE IF NOT EXISTS chunks (
        doc_id INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        start_byte INTEGER NOT NULL,
        end_byte INTEGER NOT NULL,
        first_section INTEGER NOT NULL,
        last_section INTEGER NOT NULL,
        PRIMARY KEY (doc_id, seq)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS vectors (
        doc_id INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        vector BLOB NOT NULL,
        PRIMARY KEY (doc_id, seq)
    ) WITHOUT ROWID",
];

fn schema_hash() -> String {
    datalib_store_meta::schema_hash(DDL)
}

pub struct Index {
    pool: SqlitePool,
}

impl Index {
    /// Opens the index, creating it, or emptying one written in another
    /// shape: every row is derived from the render stores, so the steps
    /// index everything again.
    pub async fn open(path: &Path) -> Result<Index> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let pool = connect(path).await?;
        match set_up(&pool).await {
            Ok(()) => Ok(Index { pool }),
            Err(e) => {
                pool.close().await;
                Err(e).with_context(|| format!("set up the search index at {}", path.display()))
            }
        }
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// `markdown_uuid → content_hash` for every document of `source_id`.
    pub async fn indexed(&self, source_id: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT markdown_uuid, content_hash FROM documents WHERE source_id = ?")
                .bind(source_id)
                .fetch_all(&self.pool)
                .await
                .context("read the indexed documents")?;
        Ok(rows.into_iter().collect())
    }

    /// Indexes `docs` in one transaction. A document already held with the
    /// same hash is left alone, vectors and all; a changed one loses its
    /// old sections, chunks and vectors.
    pub async fn put(&self, docs: &[IndexDoc]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for doc in docs {
            put_one(&mut tx, doc)
                .await
                .with_context(|| format!("index document {}", doc.markdown_uuid))?;
        }
        tx.commit().await.context("commit the indexed documents")
    }

    pub async fn remove(&self, markdown_uuids: &[String]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT doc_id FROM documents WHERE markdown_uuid IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(markdown_uuids)?)
        .fetch_all(&mut *tx)
        .await?;
        clear(&mut tx, &ids).await?;
        sqlx::query("DELETE FROM documents WHERE doc_id IN (SELECT value FROM json_each(?))")
            .bind(serde_json::to_string(&ids)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await.context("commit the removed documents")
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

async fn put_one(conn: &mut SqliteConnection, doc: &IndexDoc) -> Result<()> {
    let held: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT doc_id, source_id, content_hash FROM documents WHERE markdown_uuid = ?",
    )
    .bind(&doc.markdown_uuid)
    .fetch_optional(&mut *conn)
    .await?;
    let doc_id = match held {
        Some((_, source, hash)) if source == doc.source_id && hash == doc.content_hash => {
            return Ok(());
        }
        Some((doc_id, _, _)) => {
            clear(conn, &[doc_id]).await?;
            sqlx::query("UPDATE documents SET source_id = ?, content_hash = ? WHERE doc_id = ?")
                .bind(&doc.source_id)
                .bind(&doc.content_hash)
                .bind(doc_id)
                .execute(&mut *conn)
                .await?;
            doc_id
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO documents (source_id, markdown_uuid, content_hash) \
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
            "INSERT INTO sections (doc_id, seq, uuid, start_byte, end_byte) \
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
        sqlx::query("INSERT INTO sections_fts (rowid, title, body) VALUES (?, ?, ?)")
            .bind(section_id)
            .bind(title)
            .bind(&section.text)
            .execute(&mut *conn)
            .await?;
        at = end;
    }
    let texts: Vec<&str> = sections.iter().map(|s| s.text.as_str()).collect();
    for (seq, c) in chunks(&texts).iter().enumerate() {
        sqlx::query(
            "INSERT INTO chunks (doc_id, seq, start_byte, end_byte, first_section, last_section) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(doc_id)
        .bind(seq as i64)
        .bind(c.start as i64)
        .bind(c.end as i64)
        .bind(c.first_section as i64)
        .bind(c.last_section as i64)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Everything the index holds for these documents but their `documents` rows.
async fn clear(conn: &mut SqliteConnection, doc_ids: &[i64]) -> Result<()> {
    let ids = serde_json::to_string(doc_ids)?;
    for sql in [
        "DELETE FROM sections_fts WHERE rowid IN \
         (SELECT section_id FROM sections WHERE doc_id IN (SELECT value FROM json_each(?)))",
        "DELETE FROM sections WHERE doc_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM chunks WHERE doc_id IN (SELECT value FROM json_each(?))",
        "DELETE FROM vectors WHERE doc_id IN (SELECT value FROM json_each(?))",
    ] {
        sqlx::query(sql).bind(&ids).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // Search reads the last committed batch while a step writes the next.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        .pragma("temp_store", "memory")
        // Every source's steps write this one file, a batch at a time.
        .busy_timeout(Duration::from_secs(60));
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open the search index at {}", path.display()))
}

/// Brings the file to this build's shape under the write lock, so two
/// steps opening an old index together empty it once, and neither empties
/// what the other has started writing: the second finds the new shape.
async fn set_up(pool: &SqlitePool) -> Result<()> {
    let want = schema_hash();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let have = datalib_store_meta::schema_hash_in(&mut tx).await?;
    if have.as_ref().is_some_and(|h| *h != want) {
        tracing::info!("the search index was written in another shape; emptying it");
        drop_tables(&mut tx).await?;
    }
    for ddl in DDL {
        sqlx::query(ddl).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    datalib_store_meta::write(
        pool,
        datalib_store_meta::StoreKind::SearchIndex,
        &want,
        datalib_store_meta::Versions::default(),
    )
    .await?;
    Ok(())
}

/// Every table but `_datalib_meta`, virtual tables first so their shadow
/// tables go with them.
async fn drop_tables(conn: &mut SqliteConnection) -> Result<()> {
    for virtual_only in [true, false] {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' AND name <> '_datalib_meta' \
               AND (NOT ? OR sql LIKE 'CREATE VIRTUAL TABLE%')",
        )
        .bind(virtual_only)
        .fetch_all(&mut *conn)
        .await?;
        for name in names {
            // A name read from this file's own schema, quoted as an identifier.
            let sql = format!("DROP TABLE IF EXISTS \"{}\"", name.replace('"', "\"\""));
            sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
