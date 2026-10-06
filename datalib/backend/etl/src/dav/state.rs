//! What a DAV collection's listings mean for the store, kept in the
//! provider's raw store so it outlives a run.
//!
//! - `dav_unconfirmed`: while a whole listing is under way, the hrefs it
//!   has not named yet. It is filled from what the store holds when the
//!   listing begins and emptied as pages name things. When the listing
//!   reaches its end — in this run, or a later one that carries on from
//!   the token a cut-short run kept — what is left was deleted upstream.
//! - `dav_unstored`: objects a listing named that could not be stored
//!   (no UID, or not returned when asked), with why. A row stays until a
//!   listing names the object again or shows it gone, so an incremental
//!   run that does not mention it does not clear its problem.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::download_problems::RecordProblem;

pub const DDL: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS dav_unconfirmed (
    collection TEXT NOT NULL,
    href TEXT NOT NULL,
    PRIMARY KEY (collection, href)
)",
    "CREATE TABLE IF NOT EXISTS dav_unstored (
    collection TEXT NOT NULL,
    href TEXT NOT NULL,
    detail TEXT NOT NULL,
    PRIMARY KEY (collection, href)
)",
];

/// A whole listing of `collection` begins: everything `stored` holds,
/// and everything it could not store, is unconfirmed until named. A
/// listing already under way is abandoned for this one.
pub async fn begin_whole(pool: &SqlitePool, collection: &str, stored: &[String]) -> Result<()> {
    let mut tx = pool.begin().await.context("begin")?;
    sqlx::query("DELETE FROM dav_unconfirmed WHERE collection = ?")
        .bind(collection)
        .execute(&mut *tx)
        .await
        .context("clear an abandoned listing")?;
    for href in stored {
        sqlx::query("INSERT OR IGNORE INTO dav_unconfirmed (collection, href) VALUES (?, ?)")
            .bind(collection)
            .bind(href)
            .execute(&mut *tx)
            .await
            .context("note a stored href")?;
    }
    sqlx::query(
        "INSERT OR IGNORE INTO dav_unconfirmed (collection, href) \
         SELECT collection, href FROM dav_unstored WHERE collection = ?",
    )
    .bind(collection)
    .execute(&mut *tx)
    .await
    .context("note the unstored hrefs")?;
    tx.commit().await.context("commit")
}

/// One applied page: `listed` and `deleted` are settled, and `unstored`
/// (href, why) are what the page named and could not store.
pub async fn settle_page(
    pool: &SqlitePool,
    collection: &str,
    listed: &[String],
    deleted: &[String],
    unstored: &[(String, String)],
) -> Result<()> {
    let mut tx = pool.begin().await.context("begin")?;
    for href in listed.iter().chain(deleted) {
        for sql in [
            "DELETE FROM dav_unconfirmed WHERE collection = ? AND href = ?",
            "DELETE FROM dav_unstored WHERE collection = ? AND href = ?",
        ] {
            sqlx::query(sql)
                .bind(collection)
                .bind(href)
                .execute(&mut *tx)
                .await
                .context("settle a listed href")?;
        }
    }
    for (href, detail) in unstored {
        sqlx::query(
            "INSERT OR REPLACE INTO dav_unstored (collection, href, detail) VALUES (?, ?, ?)",
        )
        .bind(collection)
        .bind(href)
        .bind(detail)
        .execute(&mut *tx)
        .await
        .context("note an unstored href")?;
    }
    tx.commit().await.context("commit")
}

/// The listing of `collection` reached its end. Returns the hrefs a
/// whole listing never named — deleted upstream — for the provider to
/// delete; none when no whole listing was under way.
pub async fn finish_listing(pool: &SqlitePool, collection: &str) -> Result<Vec<String>> {
    let mut tx = pool.begin().await.context("begin")?;
    let gone: Vec<String> =
        sqlx::query_scalar("SELECT href FROM dav_unconfirmed WHERE collection = ? ORDER BY href")
            .bind(collection)
            .fetch_all(&mut *tx)
            .await
            .context("read the unconfirmed hrefs")?;
    sqlx::query(
        "DELETE FROM dav_unstored WHERE collection = ? \
         AND href IN (SELECT href FROM dav_unconfirmed WHERE collection = ?)",
    )
    .bind(collection)
    .bind(collection)
    .execute(&mut *tx)
    .await
    .context("drop what the listing showed gone")?;
    sqlx::query("DELETE FROM dav_unconfirmed WHERE collection = ?")
        .bind(collection)
        .execute(&mut *tx)
        .await
        .context("close the listing")?;
    tx.commit().await.context("commit")?;
    Ok(gone)
}

/// Every object of `collections` that could not be stored, as this run's
/// record problems on `table`: the whole set, so one that stores this
/// time loses its row.
pub async fn collect_unstored(
    pool: &SqlitePool,
    problems: &crate::run_problems::RunProblems,
    table: &str,
    collections: &[String],
) {
    let rows: Result<Vec<(String, String, String)>, _> =
        sqlx::query_as("SELECT collection, href, detail FROM dav_unstored ORDER BY href")
            .fetch_all(pool)
            .await;
    match rows {
        Ok(rows) => {
            problems.records_failed(
                rows.into_iter()
                    .filter(|(collection, _, _)| collections.contains(collection))
                    .map(|(_, href, detail)| RecordProblem::new(table, &href, detail)),
            );
            problems.records_tried_all(table);
        }
        Err(e) => tracing::warn!(
            error = %e,
            "dav: could not read the unstored objects; the Manage row will not show them"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for ddl in DDL {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        pool
    }

    fn hrefs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A whole listing cut short in one run and finished by the next
    /// names what it never reached only once it finishes.
    #[tokio::test]
    async fn a_whole_listing_finished_a_run_later_names_what_it_never_reached() {
        let pool = pool().await;
        begin_whole(&pool, "cal", &hrefs(&["/a", "/b", "/c"]))
            .await
            .unwrap();
        settle_page(&pool, "cal", &hrefs(&["/a"]), &[], &[])
            .await
            .unwrap();
        // Cut short: the run ends without finishing.
        settle_page(&pool, "cal", &hrefs(&["/d"]), &hrefs(&["/c"]), &[])
            .await
            .unwrap();
        assert_eq!(finish_listing(&pool, "cal").await.unwrap(), hrefs(&["/b"]));
        assert!(finish_listing(&pool, "cal").await.unwrap().is_empty());
        pool.close().await;
    }

    #[tokio::test]
    async fn an_unstored_object_stays_until_named_again_or_shown_gone() {
        let pool = pool().await;
        let why = |h: &str| (h.to_string(), "no UID".to_string());
        settle_page(
            &pool,
            "book",
            &hrefs(&["/x", "/y"]),
            &[],
            &[why("/x"), why("/y")],
        )
        .await
        .unwrap();
        settle_page(&pool, "book", &[], &[], &[]).await.unwrap();
        let left = || async {
            sqlx::query_scalar::<_, String>("SELECT href FROM dav_unstored ORDER BY href")
                .fetch_all(&pool)
                .await
                .unwrap()
        };
        assert_eq!(
            left().await,
            hrefs(&["/x", "/y"]),
            "an incremental page keeps them"
        );
        settle_page(&pool, "book", &hrefs(&["/x"]), &[], &[])
            .await
            .unwrap();
        assert_eq!(left().await, hrefs(&["/y"]), "/x was named and stored");
        begin_whole(&pool, "book", &[]).await.unwrap();
        assert_eq!(finish_listing(&pool, "book").await.unwrap(), hrefs(&["/y"]));
        assert!(
            left().await.is_empty(),
            "a whole listing without /y shows it gone"
        );
        pool.close().await;
    }
}
