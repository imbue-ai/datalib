//! Reading a doltlite store at one commit.
//!
//! A plain `SELECT` on a branch reads that branch's working set, rows the
//! writer has not committed to doltlite included. Anything reading a store
//! some other process writes reads one commit instead: it resolves a hash
//! once and opens `<store>@<hash>` read-only ([`open_at`]), a detached
//! connection on which every plain table name reads that commit
//! (docs/dev/doltlite.md#three-ways-to-read-one-commit).
//!
//! This crate is the part of that discipline with no dependencies: the
//! hash as a type, HEAD, and the two read-only opens.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// The commit a store is read at: a full hash, and nothing else.
///
/// Not `HEAD`: it resolves when the query runs rather than when the pin
/// was taken, so two reads in one pass could name two commits. Not "no
/// pin": a store with nothing committed has nothing to read, and callers
/// that find no commit decide what to do rather than fall through to
/// the working set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin(String);

/// Doltlite commit hashes are 40 lowercase hex characters, and the engine
/// rejects a shortened prefix (`ref not found`), so there is no shorter
/// form to accept.
const HASH_LEN: usize = 40;

impl Pin {
    pub fn at(commit: impl Into<String>) -> Result<Pin> {
        let commit = commit.into();
        if commit.len() != HASH_LEN
            || !commit
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f')
        {
            bail!(
                "not a doltlite commit hash: {commit:?} \
                 (want {HASH_LEN} lowercase hex characters)"
            );
        }
        Ok(Pin(commit))
    }

    pub fn commit(&self) -> &str {
        &self.0
    }

    /// The table expression that reads `table` at this commit. Safe to
    /// splice into SQL for any table name: the module name is a quoted
    /// identifier, which doltlite resolves case and all, and the hash was
    /// checked by [`Pin::at`].
    pub fn table(&self, table: &str) -> String {
        let module = format!("dolt_at_{table}").replace('"', "\"\"");
        format!("\"{module}\"('{}')", self.0)
    }
}

/// The commit this store is at now, or `None` when it has none — no
/// commits yet, or a build without the dolt extensions, which read the
/// same: nothing to pin.
pub async fn head(pool: &SqlitePool) -> Result<Option<Pin>> {
    // A table read first: a bare `dolt_hashof` answers from the session's
    // last view, so a connection held across a writer's commit would keep
    // reporting the HEAD it opened at (`a_pinned_read_names_one_commit`;
    // docs/dev/doltlite.md#what-a-read-only-connection-may-do).
    let _: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
        .fetch_one(pool)
        .await?;
    // `dolt_hashof` resolves the ref; ordering `dolt_log()` by its
    // second-resolution `date` ties on every checkpointing writer.
    let commit: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
    commit.map(Pin::at).transpose()
}

/// Something a statement named that the store, at the commit read, does
/// not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// A table, by its bare name: a `dolt_at_` module counts as the table
    /// it reads.
    Table(String),
    Column(String),
}

/// The one reading of "this store lacks what the query named": SQLite's
/// "no such table" and "no such column", doltlite's "table not found:
/// <table> at <hash>" for a table the schema has and that commit does
/// not, and sqlx's own column-not-found when a row is decoded by a column
/// the `SELECT *` did not return. `None` for anything else, so corruption
/// and bad SQL still surface as errors.
pub fn missing_schema(e: &sqlx::Error) -> Option<Missing> {
    let m = match e {
        sqlx::Error::ColumnNotFound(column) => return Some(Missing::Column(column.clone())),
        sqlx::Error::Database(db) => db.message(),
        _ => return None,
    };
    if let Some(table) = m.strip_prefix("no such table: ") {
        let table = table.strip_prefix("dolt_at_").unwrap_or(table);
        return Some(Missing::Table(table.to_string()));
    }
    if let Some((table, _commit)) = m
        .strip_prefix("table not found: ")
        .and_then(|rest| rest.split_once(" at "))
    {
        return Some(Missing::Table(table.to_string()));
    }
    m.strip_prefix("no such column: ")
        .map(|column| Missing::Column(column.to_string()))
}

/// True iff `e` says `table` is not there to read: the fresh-store state,
/// before whatever owns the table has committed it. Deliberately a match
/// on the one table the query reads.
pub fn is_missing_table(e: &sqlx::Error, table: &str) -> bool {
    matches!(missing_schema(e), Some(Missing::Table(t)) if t == table)
}

/// How long a pool waits for its one connection before giving up. Far past
/// sqlx's 30s default because a cold open of a multi-GB store legitimately
/// takes seconds inside `sqlite3_open_v2`; five minutes is "something else
/// is wrong" territory.
///
/// `DATALIB_POOL_ACQUIRE_SECS` lowers it. A caller that kills this process
/// on a deadline of its own must set it below that deadline, or a pool wait
/// is killed before sqlx can say which store it was waiting on — which is
/// the difference between a diagnosis and a silent hang.
pub fn acquire_timeout() -> Duration {
    const DEFAULT_SECS: u64 = 300;
    let secs = std::env::var("DATALIB_POOL_ACQUIRE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_SECS);
    Duration::from_secs(secs)
}

/// A read-only handle on the file's default branch, `main`: what has been
/// published, and nothing a writer still has in flight on its own branch.
/// It follows `main` as it moves; to read one commit, [`open_at`].
///
/// Read-only, so "a reader must not write" is the engine's rule rather
/// than an intention; never creates the file, because a root that has
/// not synced has none and the reader must not be what makes it. One
/// connection, never recycled: doltlite's session state is per
/// connection. The acquire timeout is [`acquire_timeout`].
pub async fn open_reader(db_path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .read_only(true)
        .create_if_missing(false);
    reader_pool(opts).await
}

/// A read-only handle on `db_path` at `pin`: doltlite opens `<file>@<hash>`
/// detached, so every plain table name on it reads that commit, the file's
/// working set is out of reach, indexes work, and no peer moving a branch
/// moves it (docs/dev/doltlite.md#opening-a-revision-by-path).
///
/// Its `sqlite_master` is the commit's schema: a table that commit does
/// not have is `no such table`, the same as one never created.
pub async fn open_at(db_path: &Path, pin: &Pin) -> Result<SqlitePool, sqlx::Error> {
    // `filename`, not a `sqlite://` URL: the `@` would parse as userinfo.
    let opts = SqliteConnectOptions::new()
        .filename(format!("{}@{}", db_path.display(), pin.commit()))
        .read_only(true)
        .create_if_missing(false);
    reader_pool(opts).await
}

/// Whether the commit a pool reads holds any table of its own. On an
/// [`open_at`] pool that is the question "is this store readable at this
/// commit": a doltlite file is born with a commit that holds nothing, and
/// a reader that took it for an empty store would report a source that
/// lost every row.
pub async fn holds_a_table(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

async fn reader_pool(opts: SqliteConnectOptions) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(acquire_timeout())
        .connect_with(opts)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "cb290c9a12e6e5c1053568c864ab582adbf42743";

    /// `HEAD` resolves when the query runs, so a pin holding it would let
    /// one pass's reads name different commits. The type refuses it.
    #[test]
    fn only_a_full_commit_hash_is_a_pin() {
        assert!(Pin::at(HASH).is_ok());
        for bad in [
            "HEAD",
            "HEAD~1",
            "cb290c9a",
            "",
            "CB290C9A12E6E5C1053568C864AB582ADBF42743",
            "cb290c9a12e6e5c1053568c864ab582adbf4274g",
            "cb290c9a12e6e5c1053568c864ab582adbf427433",
        ] {
            assert!(Pin::at(bad).is_err(), "{bad:?} was accepted as a pin");
        }
        assert_eq!(
            Pin::at(HASH).unwrap().table("grid_rows"),
            format!("\"dolt_at_grid_rows\"('{HASH}')")
        );
        assert_eq!(
            Pin::at(HASH).unwrap().table("Adobe \"x\""),
            format!("\"dolt_at_Adobe \"\"x\"\"\"('{HASH}')")
        );
    }

    /// The env override exists so a caller with a deadline shorter than
    /// the default can make a pool wait fail by name instead of being
    /// killed mid-wait. Serialised with the other env test: one process.
    #[test]
    fn the_acquire_timeout_is_overridable_and_refuses_nonsense() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = std::env::var("DATALIB_POOL_ACQUIRE_SECS").ok();
        unsafe { std::env::remove_var("DATALIB_POOL_ACQUIRE_SECS") };
        assert_eq!(acquire_timeout(), Duration::from_secs(300));
        for (set, want) in [("45", 45), ("0", 300), ("", 300), ("soon", 300)] {
            unsafe { std::env::set_var("DATALIB_POOL_ACQUIRE_SECS", set) };
            assert_eq!(
                acquire_timeout(),
                Duration::from_secs(want),
                "DATALIB_POOL_ACQUIRE_SECS={set:?}"
            );
        }
        match restore {
            Some(v) => unsafe { std::env::set_var("DATALIB_POOL_ACQUIRE_SECS", v) },
            None => unsafe { std::env::remove_var("DATALIB_POOL_ACQUIRE_SECS") },
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn writer(db: &Path) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap()
    }

    async fn commit(pool: &SqlitePool) -> Option<String> {
        sqlx::query_scalar("SELECT dolt_commit('-Am', 'c')")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn insert(pool: &SqlitePool, id: i64) {
        sqlx::query("INSERT INTO t VALUES (?)")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn count_at(pool: &SqlitePool, pin: &Pin) -> Result<i64, sqlx::Error> {
        // Safe: `Pin::table` is a literal name and a hash checked to be
        // 40 hex characters.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {}",
            pin.table("t")
        )))
        .fetch_one(pool)
        .await
    }

    /// A writer with `t` created and one row committed, or `None` on a
    /// build without the dolt extensions.
    async fn seeded(db: &Path) -> Option<SqlitePool> {
        let w = writer(db).await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&w)
            .await
            .unwrap();
        insert(&w, 1).await;
        commit(&w).await?;
        Some(w)
    }

    /// A pinned read answers for the commit it names however the working
    /// set moves after it; a fresh pin, on the same long-lived connection
    /// and with nothing read in between, sees the new commit.
    #[tokio::test]
    async fn a_pinned_read_names_one_commit() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("t.doltlite_db");
        let Some(w) = seeded(&db).await else { return };
        let r = open_reader(&db).await.unwrap();
        let first = head(&r).await.unwrap().unwrap();
        assert_eq!(count_at(&r, &first).await.unwrap(), 1);

        insert(&w, 2).await;
        let working: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&r)
            .await
            .unwrap();
        assert_eq!(working, 2, "the working set shows the uncommitted row");
        assert_eq!(count_at(&r, &first).await.unwrap(), 1, "the pin does not");
        assert_eq!(head(&r).await.unwrap().unwrap(), first);

        commit(&w).await.unwrap();
        // A `dolt_hashof` alone would still answer `first` here: nothing
        // has made this session reload the root since the commit.
        let second = head(&r).await.unwrap().unwrap();
        assert_ne!(second, first, "HEAD moved on a connection already open");
        assert_eq!(count_at(&r, &second).await.unwrap(), 2);
        assert_eq!(count_at(&r, &first).await.unwrap(), 1);
    }

    /// A table committed after the reader opened reads pinned on that same
    /// connection, with no reopen. At a commit from before it existed it
    /// reads as missing, the same as one never created.
    #[tokio::test]
    async fn a_table_committed_after_the_reader_opened_reads_without_a_reopen() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("t.doltlite_db");
        let w = writer(&db).await;
        if commit(&w).await.is_none() && head(&w).await.unwrap().is_none() {
            return;
        }
        let r = open_reader(&db).await.unwrap();
        let born = head(&r)
            .await
            .unwrap()
            .expect("a store is born with a commit");
        let e = count_at(&r, &born).await.unwrap_err();
        assert!(is_missing_table(&e, "t"), "{e}");
        assert!(!is_missing_table(&e, "u"), "{e}");

        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&w)
            .await
            .unwrap();
        insert(&w, 1).await;
        commit(&w).await.unwrap();
        let first = head(&r).await.unwrap().unwrap();
        assert_ne!(first, born);
        assert_eq!(count_at(&r, &first).await.unwrap(), 1);
        let e = count_at(&r, &born).await.unwrap_err();
        assert!(is_missing_table(&e, "t"), "{e}");
        assert!(!is_missing_table(&e, "u"), "{e}");
    }

    /// Every way a read can name what the store lacks is one answer, and
    /// a statement that is simply wrong is none.
    #[tokio::test]
    async fn a_missing_table_or_column_is_one_answer_and_bad_sql_is_not() {
        use sqlx::Row;
        let td = tempfile::tempdir().unwrap();
        let w = writer(&td.path().join("t.doltlite_db")).await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&w)
            .await
            .unwrap();
        insert(&w, 1).await;
        let err = |sql: &'static str| {
            let w = w.clone();
            async move { sqlx::query(sql).fetch_all(&w).await.unwrap_err() }
        };
        assert_eq!(
            missing_schema(&err("SELECT * FROM u").await),
            Some(Missing::Table("u".into()))
        );
        assert_eq!(
            missing_schema(&err("SELECT nope FROM t").await),
            Some(Missing::Column("nope".into()))
        );
        assert_eq!(missing_schema(&err("SELEKT 1").await), None);
        let row = sqlx::query("SELECT * FROM t").fetch_one(&w).await.unwrap();
        let decoded = row.try_get::<i64, _>("nope").unwrap_err();
        assert_eq!(
            missing_schema(&decoded),
            Some(Missing::Column("nope".into()))
        );
    }

    /// A reader must never be the thing that creates the writer's file.
    #[tokio::test]
    async fn a_reader_does_not_create_the_file() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("absent.doltlite_db");
        assert!(open_reader(&db).await.is_err());
        assert!(!db.exists());
    }
}
