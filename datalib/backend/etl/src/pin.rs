//! Which commit of a doltlite store a reader reads, and whether that commit
//! is readable at all.
//!
//! A reader opens `<store>@<hash>` read-only (`datalib_pin::open_at`, through
//! [`crate::doltlite_raw::open_reader`]), so its plain table names read that
//! commit and nothing a writer has in flight
//! (docs/dev/doltlite.md#three-ways-to-read-one-commit).
//!
//! Read P1 in `datalib/backend/dag/README.md` § "What a sink owes its
//! consumers" before a consumer treats an empty read as an empty store: "I
//! could not read this" and "there is nothing here" must stay different
//! answers, or render deletes every document the source used to have.

use anyhow::Result;

/// The commit a store is read at. The type lives in `datalib_pin`, with
/// its reasons, so the search applet and the app server pin the same way
/// without linking this crate.
pub use datalib_pin::{is_missing_table, missing_schema, Missing, Pin};

/// The commit this store is at now, or `None` when there is nothing
/// readable to pin: no commit, or none that holds a table.
///
/// For a consumer driven by [`crate::doltlite_raw::scan_buckets`], prefer the
/// `new_head` that scan already returned: the diff and the reads that follow
/// it must name one commit, and sampling HEAD a second time can pick up a
/// commit the diff did not see. This is for the consumers that do no diff at
/// all.
pub async fn head(pool: &sqlx::SqlitePool) -> Result<Option<Pin>> {
    let Some(commit) = datalib_pin::head(pool).await? else {
        // Every caller turns this into "nothing to read", which is not the
        // same as the store being empty.
        tracing::warn!(
            store = %store_path(pool).display(),
            "no commit to pin: reading this store as empty, which is not the \
             same as it being empty",
        );
        return Ok(None);
    };
    if !readable_at(&store_path(pool), &commit).await? {
        tracing::warn!(
            store = %store_path(pool).display(),
            "no commit carries this store's tables: unreadable, not empty",
        );
        return Ok(None);
    }
    Ok(Some(commit))
}

/// Whether the store's HEAD holds any of its tables.
///
/// A doltlite file is born with an "Initialize data repository" commit that
/// holds nothing, so HEAD answers for a store whose tables have never been
/// committed — a writer that died between its `CREATE TABLE` and its first
/// commit, or one a reader opened in the moment between. Read as a store,
/// that commit is a source with no rows, and a consumer that took it so
/// would sweep every document the source had.
pub async fn carries_committed_schema(pool: &sqlx::SqlitePool) -> bool {
    match datalib_pin::head(pool).await {
        Ok(Some(head)) => readable_at(&store_path(pool), &head).await.unwrap_or(false),
        _ => false,
    }
}

/// Whether `pin` holds a table, asked of the store at that commit.
pub(crate) async fn readable_at(path: &std::path::Path, pin: &Pin) -> Result<bool> {
    let at = datalib_pin::open_at(path, pin).await?;
    let holds = datalib_pin::holds_a_table(&at).await;
    at.close().await;
    Ok(holds?)
}

fn store_path(pool: &sqlx::SqlitePool) -> std::path::PathBuf {
    pool.connect_options().get_filename().to_path_buf()
}

#[cfg(test)]
mod reader_tests {
    //! Each of these guards one property a reader of somebody else's store
    //! rests on, and each fails loudly if doltlite stops behaving this way.

    use super::*;
    use crate::doltlite_raw;

    async fn count(pool: &sqlx::SqlitePool, sql: &'static str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await
    }

    // A doltlite file with nothing written into it yet, which
    // `doltlite_raw::open` never leaves behind.
    async fn bare_store(path: &std::path::Path) -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .unwrap()
    }

    /// The premise every consumer rests on: a reader sees the commit it
    /// was opened at, not the writer's working set, and a table that
    /// commit does not have is missing — never silently empty.
    #[tokio::test]
    async fn a_reader_reads_its_commit_not_the_working_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned.doltlite_db");
        let pool = doltlite_raw::open(
            &path,
            &[
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, nick TEXT)",
                "CREATE TABLE IF NOT EXISTS msgs (id TEXT PRIMARY KEY, who TEXT)",
            ],
        )
        .await
        .unwrap();
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query("INSERT INTO users VALUES ('u1', 'ann')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO msgs VALUES ('m1', 'u1')")
            .execute(&pool)
            .await
            .unwrap();
        let commit = doltlite_raw::commit_run(&pool, "seed")
            .await
            .unwrap()
            .unwrap();

        // Dirty the store the way a producer mid-run would, including a table
        // that did not exist at the commit at all.
        sqlx::query("INSERT INTO users VALUES ('u2', 'bob')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE later (id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        let reader = doltlite_raw::open_reader(&path, Some(&commit))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM users").await.unwrap(),
            2,
            "working set"
        );
        assert_eq!(
            count(&reader, "SELECT COUNT(*) FROM users").await.unwrap(),
            1,
            "the commit"
        );
        assert_eq!(
            count(
                &reader,
                "SELECT COUNT(*) FROM users u JOIN msgs m ON m.who = u.id"
            )
            .await
            .unwrap(),
            1,
            "a join reads one commit on both sides"
        );
        let e = count(&reader, "SELECT COUNT(*) FROM later")
            .await
            .unwrap_err();
        assert!(
            is_missing_table(&e, "later"),
            "a table newer than the pin: {e}"
        );
        reader.close().await;
        pool.close().await;
    }

    /// The one thing a reader can say about what it cannot see: the store
    /// is dirty, so somebody wrote rows and never sealed them. A reader at
    /// the commit cannot tell those rows from a source that holds nothing,
    /// which is how three live tests came to download a page and then
    /// assert on zero rows.
    #[tokio::test]
    async fn a_writer_that_never_sealed_leaves_the_store_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsealed.doltlite_db");
        let pool = doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        let dirty = "SELECT count(*) FROM dolt_status";
        assert_eq!(
            count(&pool, dirty).await.unwrap(),
            0,
            "the open seals itself"
        );

        sqlx::query("INSERT INTO entities VALUES ('never-sealed')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            count(&pool, dirty).await.unwrap(),
            1,
            "a row nobody committed has to be visible as dirtiness, or the \
             reader has nothing at all to warn about"
        );

        let reader = doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            count(&reader, "SELECT COUNT(*) FROM entities")
                .await
                .unwrap(),
            0,
            "the reader sees the schema commit, not the row"
        );
        reader.close().await;
        doltlite_raw::commit_run(&pool, "seal").await.unwrap();
        assert_eq!(count(&pool, dirty).await.unwrap(), 0, "sealing cleans it");
        pool.close().await;
    }

    /// `dolt_log().date` has one-second resolution, so a burst of commits --
    /// which is what checkpointing produces -- gives several rows one
    /// timestamp. Ordering by it leaves the winner to an unspecified tiebreak;
    /// `head` must name the actual HEAD every time.
    #[tokio::test]
    async fn head_tracks_head_through_a_burst_of_same_second_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("burst.doltlite_db");
        let pool = doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }

        for i in 0..8 {
            sqlx::query("INSERT INTO entities VALUES (?, 'x')")
                .bind(format!("e{i}"))
                .execute(&pool)
                .await
                .unwrap();
            let sealed = doltlite_raw::commit_run(&pool, &format!("batch {i}"))
                .await
                .unwrap()
                .unwrap();

            let reader = doltlite_raw::open_reader(&path, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                reader.pin().commit(),
                sealed,
                "head must name the commit just sealed, not a same-second sibling"
            );
            assert_eq!(
                count(&reader, "SELECT count(*) FROM entities")
                    .await
                    .unwrap(),
                i64::from(i) + 1,
                "the reader at batch {i} lost rows"
            );
            reader.close().await;
        }
        pool.close().await;
    }

    /// The window a fresh store is open in: the file exists and holds no
    /// table at all. Under streaming a producer that has just created its
    /// store is an ordinary state, so the answer must be `None` — "I cannot
    /// read this yet" — which every consumer handles by skipping the source
    /// this pass.
    #[tokio::test]
    async fn a_store_with_no_tables_at_all_is_not_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("newborn.doltlite_db");
        let pool = bare_store(&path).await;
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        assert!(
            datalib_pin::head(&pool).await.unwrap().is_some(),
            "precondition: the birth commit answers, which is why a hash \
             cannot be the readiness test"
        );
        assert!(head(&pool).await.unwrap().is_none());
        assert!(doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .is_none());
        pool.close().await;
    }

    /// The shape that deletes a source: tables written, nothing committed.
    /// HEAD is the birth commit, which holds no table, so a reader there
    /// would read zero rows and call it a completed walk.
    #[tokio::test]
    async fn a_store_whose_tables_were_never_committed_is_not_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("half.doltlite_db");
        let pool = bare_store(&path).await;
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query("CREATE TABLE entities (id TEXT PRIMARY KEY, body TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO entities VALUES ('a', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(head(&pool).await.unwrap().is_none());
        assert!(!carries_committed_schema(&pool).await);
        assert!(doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .is_none());
        pool.close().await;
    }

    /// The other half: a store that *has* committed its schema and holds no
    /// rows is readable and empty. A consumer may act on that — including
    /// deleting what the source no longer has.
    #[tokio::test]
    async fn a_committed_but_empty_store_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.doltlite_db");
        let pool = doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        assert!(carries_committed_schema(&pool).await);
        let reader = doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .expect("open commits the schema, so the store is readable");
        assert_eq!(
            count(&reader, "SELECT count(*) FROM entities")
                .await
                .unwrap(),
            0,
            "readable and empty, which is a different answer"
        );
        reader.close().await;
        pool.close().await;
    }

    /// A reader's snapshot is the connection's, so a writer sealing more
    /// after the reader opened moves nothing it reads.
    #[tokio::test]
    async fn a_reader_does_not_move_when_the_writer_seals_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moving.doltlite_db");
        let pool = doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query("INSERT INTO entities VALUES ('a')")
            .execute(&pool)
            .await
            .unwrap();
        doltlite_raw::commit_run(&pool, "one").await.unwrap();
        let reader = doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("INSERT INTO entities VALUES ('b')")
            .execute(&pool)
            .await
            .unwrap();
        doltlite_raw::commit_run(&pool, "two").await.unwrap();
        assert_eq!(
            count(&reader, "SELECT count(*) FROM entities")
                .await
                .unwrap(),
            1
        );
        let e = sqlx::query("INSERT INTO entities VALUES ('c')")
            .execute(&*reader)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("readonly"), "a reader wrote: {e}");
        reader.close().await;
        pool.close().await;
    }
}
