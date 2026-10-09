//! What a DAV collection lists, kept in the provider's raw store so it
//! outlives a run (docs/dev/data_architecture_ingestion.md, "What is left to fetch").
//!
//! - `dav_resources`: one row per object a listing named as present,
//!   with the etag it was listed at. Its `_bookkeeping` sidecar's
//!   `held_version` is the etag the stored object satisfies, written in
//!   the transaction that stored it (`crate::owed::hold`). What is owed
//!   is the difference, asked each time and never stored, so an object a
//!   listing named and a `multiget` did not return stays owed after the
//!   token moves on. The content itself is the provider's (a `contacts`
//!   row, an `ics_objects` row), keyed by the object's UID; this table is
//!   keyed by href because the listing knows nothing else.
//! - `dav_unconfirmed`: while a whole listing is under way, the hrefs it
//!   has not named yet. Filled from `dav_resources` when the listing
//!   begins and emptied as pages name things; when the listing reaches
//!   its end, in this run or a later one that carries on from the token
//!   a cut-short run kept, what is left was deleted upstream.

use std::collections::HashSet;

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::owed::{self, Listed};
use datalib_etl::doltlite_raw as dr;

pub const RESOURCES: &str = "dav_resources";

const RESOURCES_DDL: &str = "CREATE TABLE IF NOT EXISTS dav_resources (
    id TEXT PRIMARY KEY,
    collection TEXT NOT NULL,
    href TEXT NOT NULL,
    etag TEXT NULL
)";

const RESOURCES_BY_COLLECTION_DDL: &str =
    "CREATE INDEX IF NOT EXISTS dav_resources_by_collection ON dav_resources(collection)";

const UNCONFIRMED_DDL: &str = "CREATE TABLE IF NOT EXISTS dav_unconfirmed (
    collection TEXT NOT NULL,
    href TEXT NOT NULL,
    PRIMARY KEY (collection, href)
)";

/// Every table here, the listing's sidecar included.
pub fn ddl() -> Vec<String> {
    vec![
        RESOURCES_DDL.to_string(),
        RESOURCES_BY_COLLECTION_DDL.to_string(),
        dr::bookkeeping_ddl_for(RESOURCES),
        UNCONFIRMED_DDL.to_string(),
    ]
}

/// The `dav_resources` key of an object: its collection's id, then its
/// href. Both may hold any character, so nothing parses this back; a
/// caller that needs the href keeps it beside the key.
pub fn resource_id(collection: &str, href: &str) -> String {
    format!("{collection}#{href}")
}

/// An object a listing named: where it is and the version it is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub href: String,
    pub etag: Option<String>,
}

impl Resource {
    pub fn listed(&self, collection: &str) -> Listed {
        Listed::new(resource_id(collection, &self.href), self.etag.as_deref())
    }
}

/// A page's objects, as listed: each row's etag follows the listing.
/// Nothing is stamped as fetched here; that is [`owed::hold`]'s, in the
/// transaction that stores the object.
pub async fn list(
    tx: &mut Transaction<'_, Sqlite>,
    collection: &str,
    resources: &[Resource],
) -> Result<()> {
    for r in resources {
        sqlx::query(
            "INSERT INTO dav_resources (id, collection, href, etag) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET etag = excluded.etag",
        )
        .bind(resource_id(collection, &r.href))
        .bind(collection)
        .bind(&r.href)
        .bind(&r.etag)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("list {}", r.href))?;
    }
    Ok(())
}

/// Everything `collection` lists, by href.
pub async fn resources(pool: &SqlitePool, collection: &str) -> Result<Vec<Resource>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT href, etag FROM dav_resources WHERE collection = ? ORDER BY href")
            .bind(collection)
            .fetch_all(pool)
            .await
            .context("read what the collection lists")?;
    Ok(rows
        .into_iter()
        .map(|(href, etag)| Resource { href, etag })
        .collect())
}

/// Of what `collection` lists, the objects not held at their listed
/// etag, by href.
pub async fn owed(pool: &SqlitePool, collection: &str) -> Result<Vec<Resource>> {
    let listed = resources(pool, collection).await?;
    let keys: Vec<Listed> = listed.iter().map(|r| r.listed(collection)).collect();
    let owed: HashSet<String> = owed::owed(pool, RESOURCES, keys)
        .await?
        .into_iter()
        .map(|o| o.key)
        .collect();
    Ok(listed
        .into_iter()
        .filter(|r| owed.contains(&resource_id(collection, &r.href)))
        .collect())
}

/// Upstream no longer has the object at `href`: its listing row, its
/// sidecar and its fetch problem go, in the transaction that removes
/// the provider's content for it.
pub async fn forget(tx: &mut Transaction<'_, Sqlite>, collection: &str, href: &str) -> Result<()> {
    unlist(tx, collection, href).await?;
    owed::forget(tx, RESOURCES, &resource_id(collection, href)).await
}

/// The listing row alone; for a loop that drops the sidecar itself.
pub async fn unlist(tx: &mut Transaction<'_, Sqlite>, collection: &str, href: &str) -> Result<()> {
    sqlx::query("DELETE FROM dav_resources WHERE id = ?")
        .bind(resource_id(collection, href))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("unlist {href}"))?;
    Ok(())
}

/// Upstream no longer has `collection` at all: everything it listed,
/// the sidecars, the fetch problems and any listing under way go, in
/// the transaction that removes the provider's content for it.
pub async fn forget_collection(tx: &mut Transaction<'_, Sqlite>, collection: &str) -> Result<()> {
    for sql in [
        "DELETE FROM problems WHERE scope_kind = ? AND scope_key IN \
         (SELECT 'dav_resources:' || id FROM dav_resources WHERE collection = ?)",
        "DELETE FROM dav_resources_bookkeeping WHERE id IN \
         (SELECT id FROM dav_resources WHERE collection = ?)",
        "DELETE FROM dav_resources WHERE collection = ?",
        "DELETE FROM dav_unconfirmed WHERE collection = ?",
    ] {
        let mut q = sqlx::query(sql);
        if sql.contains("scope_kind") {
            q = q.bind(datalib_problems::ScopeKind::Entity.as_str());
        }
        q.bind(collection)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("forget what {collection} listed"))?;
    }
    Ok(())
}

/// A whole listing of `collection` begins: everything it lists is
/// unconfirmed until named. A listing already under way is abandoned
/// for this one.
pub async fn begin_whole(tx: &mut Transaction<'_, Sqlite>, collection: &str) -> Result<()> {
    sqlx::query("DELETE FROM dav_unconfirmed WHERE collection = ?")
        .bind(collection)
        .execute(&mut **tx)
        .await
        .context("clear an abandoned listing")?;
    sqlx::query(
        "INSERT OR IGNORE INTO dav_unconfirmed (collection, href) \
         SELECT collection, href FROM dav_resources WHERE collection = ?",
    )
    .bind(collection)
    .execute(&mut **tx)
    .await
    .context("note what the collection lists")?;
    Ok(())
}

/// Of `ids`, the ones whose object the mirror holds whole: a fetch
/// landed and nothing of it was lost. What tells a new object from an
/// updated one in a run's summary.
pub async fn stored(pool: &SqlitePool, ids: &[String]) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for chunk in ids.chunks(datalib_etl::bulk::SQL_CHUNK) {
        // Audited: one `?` per id, every id bound.
        let sql = format!(
            "SELECT id FROM dav_resources_bookkeeping \
             WHERE fetched_at_utc IS NOT NULL AND last_error IS NULL AND id IN ({})",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            q = q.bind(id);
        }
        out.extend(
            q.fetch_all(pool)
                .await
                .context("read what the mirror holds whole")?,
        );
    }
    Ok(out)
}

/// A page named these hrefs, present or deleted: they are settled.
pub async fn settle(
    tx: &mut Transaction<'_, Sqlite>,
    collection: &str,
    hrefs: &[&str],
) -> Result<()> {
    for href in hrefs {
        sqlx::query("DELETE FROM dav_unconfirmed WHERE collection = ? AND href = ?")
            .bind(collection)
            .bind(href)
            .execute(&mut **tx)
            .await
            .context("settle a listed href")?;
    }
    Ok(())
}

/// The listing of `collection` reached its end. Returns the hrefs a
/// whole listing never named — deleted upstream — for the provider to
/// forget; none when no whole listing was under way.
pub async fn finish_listing(
    tx: &mut Transaction<'_, Sqlite>,
    collection: &str,
) -> Result<Vec<String>> {
    let gone: Vec<String> =
        sqlx::query_scalar("SELECT href FROM dav_unconfirmed WHERE collection = ? ORDER BY href")
            .bind(collection)
            .fetch_all(&mut **tx)
            .await
            .context("read the unconfirmed hrefs")?;
    sqlx::query("DELETE FROM dav_unconfirmed WHERE collection = ?")
        .bind(collection)
        .execute(&mut **tx)
        .await
        .context("close the listing")?;
    Ok(gone)
}

/// A rung for a store from before `dav_resources`: every object `table`
/// holds from a server (an href and an etag; a `.vcf` or `.ics` file's
/// rows have no etag) is listed at that etag and held at it, with its
/// attempts carried over, so nothing is fetched again. What the old
/// `dav_unstored` table named is listed with no etag and not held, so
/// it is asked for next run; its `record:` problem rows go, since the
/// next attempt writes the row that stands for it now. `collection_col`
/// is `table`'s column naming the collection.
pub async fn adopt(
    conn: &mut SqliteConnection,
    table: &'static str,
    collection_col: &'static str,
) -> Result<()> {
    for stmt in ddl() {
        // Audited: this module's own DDL.
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(&mut *conn)
            .await?;
    }
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    // Audited: `table` and `collection_col` are the calling rung's
    // `&'static str`; values bound.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT OR IGNORE INTO dav_resources (id, collection, href, etag) \
         SELECT {collection_col} || '#' || href, {collection_col}, href, etag FROM {table} \
         WHERE href IS NOT NULL AND etag IS NOT NULL"
    )))
    .execute(&mut *conn)
    .await
    .context("list what the store holds")?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT OR IGNORE INTO dav_resources_bookkeeping \
            (id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, \
             volatile_payload, tz_offset, held_version) \
         SELECT t.{collection_col} || '#' || t.href, COALESCE(b.fetched_at_utc, ?), \
                COALESCE(b.attempt_count, 1), b.last_attempt_at_utc, NULL, NULL, \
                COALESCE(b.tz_offset, ?), t.etag \
         FROM {table} t LEFT JOIN {table}_bookkeeping b ON b.id = t.id \
         WHERE t.href IS NOT NULL AND t.etag IS NOT NULL"
    )))
    .bind(&now)
    .bind(&tz_offset)
    .execute(&mut *conn)
    .await
    .context("hold what the store holds")?;

    let has_unstored: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'dav_unstored')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !has_unstored {
        return Ok(());
    }
    sqlx::query(
        "INSERT OR IGNORE INTO dav_resources (id, collection, href, etag) \
         SELECT collection || '#' || href, collection, href, NULL FROM dav_unstored",
    )
    .execute(&mut *conn)
    .await
    .context("list what could not be stored")?;
    sqlx::query(
        "INSERT OR IGNORE INTO dav_resources_bookkeeping \
            (id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, \
             volatile_payload, tz_offset, held_version) \
         SELECT collection || '#' || href, NULL, 1, ?, detail, NULL, ?, NULL FROM dav_unstored",
    )
    .bind(&now)
    .bind(&tz_offset)
    .execute(&mut *conn)
    .await
    .context("owe what could not be stored")?;
    let has_problems: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'problems')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if has_problems {
        sqlx::query("DELETE FROM problems WHERE instr(scope_key, ?) = 1")
            .bind(format!("record:{table}:"))
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("DROP TABLE dav_unstored")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, SqlitePool) {
        let d = tempfile::tempdir().unwrap();
        let ddl = ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = dr::open(&d.path().join("s.doltlite_db"), &ddl)
            .await
            .unwrap();
        (d, pool)
    }

    fn r(href: &str, etag: &str) -> Resource {
        Resource {
            href: href.into(),
            etag: Some(etag.into()),
        }
    }

    fn hrefs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    async fn listed_then_held(pool: &SqlitePool, collection: &str, held: &[Resource]) {
        let mut tx = pool.begin().await.unwrap();
        list(&mut tx, collection, held).await.unwrap();
        for h in held {
            owed::hold(
                &mut tx,
                RESOURCES,
                &resource_id(collection, &h.href),
                h.etag.as_deref(),
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
    }

    /// A whole listing cut short in one run and finished by the next
    /// names what it never reached only once it finishes.
    #[tokio::test]
    async fn a_whole_listing_finished_a_run_later_names_what_it_never_reached() {
        let (_d, pool) = store().await;
        listed_then_held(&pool, "cal", &[r("/a", "1"), r("/b", "1"), r("/c", "1")]).await;
        let mut tx = pool.begin().await.unwrap();
        begin_whole(&mut tx, "cal").await.unwrap();
        settle(&mut tx, "cal", &["/a"]).await.unwrap();
        tx.commit().await.unwrap();
        // Cut short: the run ends without finishing.
        let mut tx = pool.begin().await.unwrap();
        settle(&mut tx, "cal", &["/d", "/c"]).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            finish_listing(&mut tx, "cal").await.unwrap(),
            hrefs(&["/b"])
        );
        assert!(finish_listing(&mut tx, "cal").await.unwrap().is_empty());
        tx.commit().await.unwrap();
        pool.close().await;
    }

    /// An object is owed until it is held at the etag it is listed at:
    /// listed and never fetched, listed again at a new etag, or fetched
    /// and failed. A listing that names it at the etag it is held at
    /// owes nothing, and one that forgets it owes nothing for it.
    #[tokio::test]
    async fn an_object_is_owed_until_held_at_its_listed_etag() {
        let (_d, pool) = store().await;
        listed_then_held(&pool, "book", &[r("/x", "1"), r("/y", "1")]).await;
        assert!(owed(&pool, "book").await.unwrap().is_empty());

        let mut tx = pool.begin().await.unwrap();
        list(&mut tx, "book", &[r("/x", "2"), r("/z", "1")])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            owed(&pool, "book").await.unwrap(),
            [r("/x", "2"), r("/z", "1")],
            "an etag that moved, and an object never fetched"
        );

        let mut tx = pool.begin().await.unwrap();
        dr::record_object_error(
            &mut tx,
            RESOURCES,
            &resource_id("book", "/z"),
            "not returned",
        )
        .await
        .unwrap();
        forget(&mut tx, "book", "/x").await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            owed(&pool, "book").await.unwrap(),
            [r("/z", "1")],
            "a failed fetch leaves it owed; a forgotten object is not"
        );
        let keys: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            keys,
            [format!("dav_resources:{}", resource_id("book", "/z"))]
        );
        pool.close().await;
    }
}
