//! Reading Mail's Envelope Index: a snapshot of it, so Mail can go on
//! writing while a sync reads, then the rows [`super::plan`] decides from.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::Row as _;

use super::plan::{IndexRows, MailboxRow, MessageRow};

pub async fn read(index: &Path) -> Result<IndexRows> {
    let snapshot = datalib_etl_sqlite_mirror::snapshot(index)
        .await
        .with_context(|| format!("snapshot {}", index.display()))?;
    let pool = datalib_etl_sqlite_mirror::open_sqlite(snapshot.path(), false).await?;
    let rows = read_rows(&pool).await;
    pool.close().await;
    rows
}

async fn read_rows(pool: &sqlx::SqlitePool) -> Result<IndexRows> {
    let mailboxes = sqlx::query("SELECT ROWID AS rowid, url FROM mailboxes")
        .fetch_all(pool)
        .await
        .context("read Mail's mailboxes")?
        .into_iter()
        .map(|r| MailboxRow {
            rowid: r.get("rowid"),
            url: r.get("url"),
        })
        .collect();
    let messages = sqlx::query(
        "SELECT m.ROWID AS rowid, m.mailbox, m.conversation_id, m.read, m.flagged, m.deleted,
                m.flags, m.date_received, g.message_id_header
         FROM messages m
         LEFT JOIN message_global_data g ON g.ROWID = m.global_message_id",
    )
    .fetch_all(pool)
    .await
    .context("read Mail's messages")?
    .into_iter()
    .map(|r| MessageRow {
        rowid: r.get("rowid"),
        mailbox: r.get("mailbox"),
        message_id: r.get("message_id_header"),
        conversation_id: r.get("conversation_id"),
        read: r.get::<i64, _>("read") != 0,
        flagged: r.get::<i64, _>("flagged") != 0,
        deleted: r.get::<i64, _>("deleted") != 0,
        flags: r.get("flags"),
        date_received: r.get("date_received"),
    })
    .collect();
    let labels = sqlx::query("SELECT message_id, mailbox_id FROM labels")
        .fetch_all(pool)
        .await
        .context("read Mail's labels")?
        .into_iter()
        .map(|r| (r.get("message_id"), r.get("mailbox_id")))
        .collect();
    Ok(IndexRows {
        mailboxes,
        messages,
        labels,
    })
}
