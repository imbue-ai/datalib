//! The people the documents carry, as the grid index holds them: each
//! distinct record of a person once, and a narrow row for each document
//! that carries it. A render store keeps a whole record per document
//! (`datalib_schema::source_contacts`), so a person in 30,000 email
//! threads has 30,000 copies that differ only in how much they wrote in
//! each; here that count is the document's row and the rest is stored
//! once.

use anyhow::{Context, Result};
use datalib_contact_schema::NormalizedContact;
use datalib_sql::{sql, sql_id, Sql};
use sqlx::SqliteConnection;

/// `contact_json` is the record with no `seen`, which is the document's:
/// null `seen_items` says the source counts nothing.
pub(super) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS contact_records (record_id INTEGER PRIMARY KEY, \
     source_id TEXT NOT NULL, contact_key TEXT NOT NULL, name TEXT, \
     contact_json TEXT NOT NULL UNIQUE)",
    "CREATE TABLE IF NOT EXISTS contact_record_handles (record_id INTEGER NOT NULL, \
     handle TEXT NOT NULL, PRIMARY KEY (record_id, handle)) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS document_contacts (markdown_uuid TEXT NOT NULL, \
     record_id INTEGER NOT NULL, seen_items INTEGER, last_seen_at TEXT, \
     PRIMARY KEY (markdown_uuid, record_id)) WITHOUT ROWID",
];

pub(super) const INDEXES: &[&str] = &[
    // A chip asks who a handle is.
    "CREATE INDEX IF NOT EXISTS contact_record_handles_by_handle \
     ON contact_record_handles (handle)",
    // And how much they wrote, over every document that carries them.
    "CREATE INDEX IF NOT EXISTS document_contacts_by_record \
     ON document_contacts (record_id, seen_items, last_seen_at)",
];

/// A record goes when the last document that carries it does. Until a
/// pass ends with this, one nothing carries is there to be joined to
/// nothing.
pub(super) const ORPHANED: &[&str] = &[
    "DELETE FROM contact_record_handles WHERE NOT EXISTS (SELECT 1 \
     FROM document_contacts d INDEXED BY document_contacts_by_record \
     WHERE d.record_id = contact_record_handles.record_id)",
    "DELETE FROM contact_records WHERE NOT EXISTS (SELECT 1 \
     FROM document_contacts d INDEXED BY document_contacts_by_record \
     WHERE d.record_id = contact_records.record_id)",
];

/// Record that `markdown_uuid` carries `contact`.
pub(super) async fn insert(
    conn: &mut SqliteConnection,
    markdown_uuid: &str,
    contact: &NormalizedContact,
) -> Result<()> {
    let record_id = record_id(conn, contact)
        .await
        .with_context(|| format!("record source contact {}", contact.key))?;
    let seen = contact.seen.as_ref();
    sql!(
        "etl.render.grid.upsert_document_contact",
        "INSERT OR REPLACE INTO document_contacts \
         (markdown_uuid, record_id, seen_items, last_seen_at) VALUES (?, ?, ?, ?)"
    )
    .query()
    .bind(markdown_uuid)
    .bind(record_id)
    .bind(seen.map(|s| s.items as i64))
    .bind(seen.and_then(|s| s.last_at.as_deref()))
    .execute(&mut *conn)
    .await
    .with_context(|| format!("insert source contact {}", contact.key))?;
    Ok(())
}

async fn record_id(conn: &mut SqliteConnection, contact: &NormalizedContact) -> Result<i64> {
    let json = serde_json::to_string(&NormalizedContact {
        seen: None,
        ..contact.clone()
    })
    .context("serialize a source contact")?;
    let held: Option<i64> = sql!(
        "etl.render.grid.find_contact_record_by_json",
        "SELECT record_id FROM contact_records WHERE contact_json = ?"
    )
    .query_scalar()
    .bind(&json)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(held) = held {
        return Ok(held);
    }
    let record_id: i64 = sql!(
        "etl.render.grid.insert_contact_record",
        "INSERT INTO contact_records (source_id, contact_key, name, contact_json) \
         VALUES (?, ?, ?, ?) RETURNING record_id"
    )
    .query_scalar()
    .bind(&contact.source_id)
    .bind(&contact.key)
    .bind(contact.name())
    .bind(&json)
    .fetch_one(&mut *conn)
    .await?;
    for handle in contact.handles.iter().filter_map(|h| h.handle.as_ref()) {
        sql!(
            "etl.render.grid.insert_contact_record_handle",
            "INSERT OR IGNORE INTO contact_record_handles (record_id, handle) VALUES (?, ?)"
        )
        .query()
        .bind(record_id)
        .bind(handle.as_str())
        .execute(&mut *conn)
        .await
        .with_context(|| format!("insert handle {handle}"))?;
    }
    Ok(record_id)
}

/// Drop the records no document carries any more.
pub(crate) async fn sweep(conn: &mut SqliteConnection) -> Result<()> {
    for &sql in ORPHANED {
        Sql::dynamic(
            sql_id!("etl.render.grid.delete_uncarried_contact_records"),
            sql,
        )
        .query()
        .execute(&mut *conn)
        .await
        .context("drop the contact records no document carries")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_index::source_cursor_tests::doc;
    use crate::grid_index::{apply_one, delete_markdowns, open_index, seal, WriteLock};
    use datalib_contact_schema::{ContactHandle, ContactKind, Seen};

    use datalib_sql::{test_sql, test_sql_id, Sql};
    fn riker(names: &[&str], seen: Option<(u64, &str)>) -> NormalizedContact {
        let mut c = NormalizedContact::new("src", "U_RIKER", ContactKind::Person);
        c.names = names.iter().map(|n| n.to_string()).collect();
        c.handles = vec![
            ContactHandle::email(None, "riker@enterprise.org"),
            ContactHandle::phone(None, "+1 202 555 0101"),
        ];
        c.seen = seen.map(|(items, at)| Seen {
            items,
            last_at: Some(at.to_string()),
        });
        c
    }

    async fn count(pool: &sqlx::SqlitePool, table: &str) -> i64 {
        // Audited: `table` is a literal of this test.
        Sql::dynamic(test_sql_id!(), format!("SELECT count(*) FROM {table}"))
            .query_scalar()
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// (records, their handles, documents carrying one).
    async fn held(pool: &sqlx::SqlitePool) -> (i64, i64, i64) {
        (
            count(pool, "contact_records").await,
            count(pool, "contact_record_handles").await,
            count(pool, "document_contacts").await,
        )
    }

    /// The index held a whole copy of a person's record for every document
    /// that carried it: 309,481 copies of 2,081 records in one library.
    /// Copies that differ in how much the person wrote are one record; a
    /// name shown differently is another. A record goes, with its
    /// handles, when the last document that carries it does.
    #[tokio::test]
    async fn a_record_is_held_once_and_goes_with_its_last_document() {
        let td = tempfile::tempdir().unwrap();
        let pool = open_index(&td.path().join("db.sqlite")).await.unwrap();
        let lock = WriteLock::for_index(pool.clone());
        let carrying = |uuid: &str, contact: NormalizedContact| {
            let mut d = doc("src", uuid, "body");
            d.contacts = vec![contact];
            d
        };
        for md in [
            carrying("md-a", riker(&["Will Riker"], Some((2, "2369-01-01")))),
            carrying("md-b", riker(&["Will Riker"], Some((7, "2369-05-01")))),
            carrying("md-c", riker(&["Will Riker"], None)),
            carrying("md-d", riker(&["Number One"], Some((1, "2369-02-01")))),
        ] {
            apply_one(&lock, &md).await.unwrap();
        }
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (2, 4, 4));

        delete_markdowns(&lock, &["md-a".into(), "md-b".into(), "md-d".into()])
            .await
            .unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (1, 2, 1), "md-c still carries one");

        apply_one(&lock, &carrying("md-c", riker(&["William"], None)))
            .await
            .unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (1, 2, 1), "a re-render replaces it");
        let names: Vec<String> = test_sql!("SELECT name FROM contact_records")
            .query_scalar()
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(names, ["William"]);

        delete_markdowns(&lock, &["md-c".into()]).await.unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (0, 0, 0));
        pool.close().await;
    }
}
