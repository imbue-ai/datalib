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
use sqlx::{Row, SqliteConnection};

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
    // A number a record writes without its country code, which makes no
    // handle: its digits (`datalib_handle::home_digits`).
    "CREATE TABLE IF NOT EXISTS contact_record_numbers (record_id INTEGER NOT NULL, \
     digits TEXT NOT NULL, PRIMARY KEY (record_id, digits)) WITHOUT ROWID",
];

pub(super) const INDEXES: &[&str] = &[
    // A chip asks who a handle is.
    "CREATE INDEX IF NOT EXISTS contact_record_handles_by_handle \
     ON contact_record_handles (handle)",
    // And how much they wrote, over every document that carries them.
    "CREATE INDEX IF NOT EXISTS document_contacts_by_record \
     ON document_contacts (record_id, seen_items, last_seen_at)",
    // The numbers that may be one, read by the last digits they share
    // (`datalib_handle::SHARED_DIGITS`): the full ones, and those without
    // a country code.
    "CREATE INDEX IF NOT EXISTS contact_record_handles_by_last_digits \
     ON contact_record_handles (substr(handle, -7)) WHERE substr(handle, 1, 4) = 'tel:'",
    "CREATE INDEX IF NOT EXISTS contact_record_numbers_by_last_digits \
     ON contact_record_numbers (substr(digits, -7))",
];

/// A record goes when the last document that carries it does. Until a
/// pass ends with this, one nothing carries is there to be joined to
/// nothing.
pub(super) const ORPHANED: &[&str] = &[
    "DELETE FROM contact_record_handles WHERE NOT EXISTS (SELECT 1 \
     FROM document_contacts d INDEXED BY document_contacts_by_record \
     WHERE d.record_id = contact_record_handles.record_id)",
    "DELETE FROM contact_record_numbers WHERE NOT EXISTS (SELECT 1 \
     FROM document_contacts d INDEXED BY document_contacts_by_record \
     WHERE d.record_id = contact_record_numbers.record_id)",
    "DELETE FROM contact_records WHERE NOT EXISTS (SELECT 1 \
     FROM document_contacts d INDEXED BY document_contacts_by_record \
     WHERE d.record_id = contact_records.record_id)",
];

/// Record that each document carries each contact, `(markdown_uuid,
/// contact)`: a record new to the index with its handles and numbers, and
/// a row for each document that carries it. Two accounts of one person in
/// one document are one row, the later winning.
pub(super) async fn insert_page(
    conn: &mut SqliteConnection,
    carried: &[(&str, &NormalizedContact)],
) -> Result<()> {
    let jsons = carried
        .iter()
        .map(|(_, c)| {
            serde_json::to_string(&NormalizedContact {
                seen: None,
                ..(*c).clone()
            })
            .context("serialize a source contact")
        })
        .collect::<Result<Vec<String>>>()?;
    let mut seen = std::collections::HashSet::new();
    let records: Vec<(&NormalizedContact, &String)> = carried
        .iter()
        .zip(&jsons)
        .filter(|(_, json)| seen.insert(json.as_str()))
        .map(|((_, c), json)| (*c, json))
        .collect();
    let made: std::collections::HashMap<String, i64> = datalib_sql::values::fetch(
        conn,
        sql_id!("etl.render.grid.insert_contact_records"),
        "INSERT INTO contact_records (source_id, contact_key, name, contact_json)",
        4,
        "ON CONFLICT (contact_json) DO NOTHING RETURNING contact_json, record_id",
        &records,
        |q, (c, json)| q.bind(&c.source_id).bind(&c.key).bind(c.name()).bind(*json),
    )
    .await
    .context("record the source contacts")?
    .iter()
    .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
    .collect::<Result<_, sqlx::Error>>()?;
    let new: Vec<(&NormalizedContact, i64)> = records
        .iter()
        .filter_map(|(c, json)| Some((*c, *made.get(json.as_str())?)))
        .collect();
    let handles: Vec<(i64, &str)> = new
        .iter()
        .flat_map(|(c, id)| {
            c.handles
                .iter()
                .filter_map(|h| h.handle.as_ref())
                .map(move |h| (*id, h.as_str()))
        })
        .collect();
    datalib_sql::values::fetch(
        conn,
        sql_id!("etl.render.grid.insert_contact_record_handles"),
        "INSERT OR IGNORE INTO contact_record_handles (record_id, handle)",
        2,
        "",
        &handles,
        |q, (id, handle)| q.bind(*id).bind(*handle),
    )
    .await
    .context("insert the records' handles")?;
    // A number a record writes without its country code makes no handle.
    let numbers: Vec<(i64, String)> = new
        .iter()
        .flat_map(|(c, id)| {
            c.handles
                .iter()
                .filter_map(|h| Some((*id, h.home_digits()?)))
        })
        .collect();
    datalib_sql::values::fetch(
        conn,
        sql_id!("etl.render.grid.insert_record_numbers"),
        "INSERT OR IGNORE INTO contact_record_numbers (record_id, digits)",
        2,
        "",
        &numbers,
        |q, (id, digits)| q.bind(*id).bind(digits),
    )
    .await
    .context("insert the numbers without their country code")?;
    let ids: std::collections::HashMap<String, i64> = sql!(
        "etl.render.grid.find_contact_records_by_json",
        "SELECT contact_json, record_id FROM contact_records \
          WHERE contact_json IN (SELECT value FROM json_each(?))"
    )
    .query_as::<(String, i64)>()
    .bind(serde_json::to_string(
        &records
            .iter()
            .map(|(_, json)| json.as_str())
            .collect::<Vec<_>>(),
    )?)
    .fetch_all(&mut *conn)
    .await
    .context("look up the source contacts' records")?
    .into_iter()
    .collect();
    let carrying = carried
        .iter()
        .zip(&jsons)
        .map(|((md, c), json)| {
            let id = ids
                .get(json)
                .with_context(|| format!("no record for source contact {}", c.key))?;
            Ok((*md, *id, *c))
        })
        .collect::<Result<Vec<_>>>()?;
    datalib_sql::values::fetch(
        conn,
        sql_id!("etl.render.grid.upsert_document_contacts"),
        "INSERT OR REPLACE INTO document_contacts \
         (markdown_uuid, record_id, seen_items, last_seen_at)",
        4,
        "",
        &carrying,
        |q, (md, id, c)| {
            let seen = c.seen.as_ref();
            q.bind(*md)
                .bind(*id)
                .bind(seen.map(|s| s.items as i64))
                .bind(seen.and_then(|s| s.last_at.as_deref()))
        },
    )
    .await
    .context("record which documents carry them")?;
    Ok(())
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
            ContactHandle::phone(None, "(907) 555-0142"),
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

    /// (records, their handles, their numbers with no country code,
    /// documents carrying one).
    async fn held(pool: &sqlx::SqlitePool) -> (i64, i64, i64, i64) {
        (
            count(pool, "contact_records").await,
            count(pool, "contact_record_handles").await,
            count(pool, "contact_record_numbers").await,
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
        assert_eq!(held(&pool).await, (2, 4, 2, 4));

        delete_markdowns(&lock, &["md-a".into(), "md-b".into(), "md-d".into()])
            .await
            .unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (1, 2, 1, 1), "md-c still carries one");

        apply_one(&lock, &carrying("md-c", riker(&["William"], None)))
            .await
            .unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (1, 2, 1, 1), "a re-render replaces it");
        let names: Vec<String> = test_sql!("SELECT name FROM contact_records")
            .query_scalar()
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(names, ["William"]);

        delete_markdowns(&lock, &["md-c".into()]).await.unwrap();
        seal(&lock).await.unwrap();
        assert_eq!(held(&pool).await, (0, 0, 0, 0));
        pool.close().await;
    }
}
