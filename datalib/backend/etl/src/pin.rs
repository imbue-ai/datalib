//! Reading a doltlite store at a pinned commit.
//!
//! A plain `SELECT` reads doltlite's working set, which lives in the file and
//! is shared across processes, so it can return rows a writer has not
//! committed yet. A consumer reading a store whose producer is still running
//! must read committed state instead: `dolt_at_<table>('<hash>')`.
//!
//! [`install_views`] does that once per connection rather than once per query,
//! by creating a `pinned_<table>` view over each table. Queries then read
//! `pinned_users` where they used to read `users`, and are otherwise
//! untouched — no pin has to be threaded down to the code running the query.
//!
//! **Why the views are named distinctly instead of shadowing the tables.** A
//! temp view named `users` would shadow the real `users`, and every existing
//! query would be pinned with no edit at all. It would also mean a pass that
//! forgot to install the views silently read the working set. A distinct name
//! turns that into `no such table: pinned_users` — loud and immediate rather
//! than a quiet wrong answer. It also leaves writes through the real names
//! working, so a pool that reads and writes is unaffected.
//!
//! See `docs/dev/streaming_steps_plan.md`.

use anyhow::{bail, Result};

/// Prefix for the per-connection pinned views: `users` is read as
/// `pinned_users`.
pub const VIEW_PREFIX: &str = "pinned_";

/// The commit a store is read at.
///
/// **Holds a full commit hash and nothing else.** Not `HEAD`, deliberately:
/// `HEAD` resolves when the query runs rather than when the pin was taken, so
/// a pin that could carry it would let one pass's diff and its content reads
/// name two different commits — which is the exact race streaming introduces.
/// Making that unrepresentable is most of what this type is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// No commit to read at: a store with no commits, or a build with no dolt
    /// extensions. Views over it read the working set — see [`install_views`].
    Unpinned,
    At(String),
}

/// Doltlite commit hashes are 40 lowercase hex characters, and the engine
/// rejects a shortened prefix (`ref not found`), so there is no shorter form
/// to accept.
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
        Ok(Pin::At(commit))
    }

    /// The pin a scan produced, where `None` is a store the scan could not
    /// name a commit for — no commits yet, or no dolt extensions at all.
    pub fn from_scan(commit: Option<&str>) -> Result<Pin> {
        match commit {
            Some(c) => Pin::at(c),
            None => Ok(Pin::Unpinned),
        }
    }

    pub fn commit(&self) -> Option<&str> {
        match self {
            Pin::Unpinned => None,
            Pin::At(c) => Some(c),
        }
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self, Pin::At(_))
    }
}

/// Create one `pinned_<table>` view per table on this connection, and return
/// how many. Call it once, when a store is opened for reading.
///
/// The views are temp-schema objects, so they last exactly as long as the
/// connection, are invisible to every other reader of the file, and write
/// nothing to it. Our pools are size 1 with recycling disabled (see
/// `doltlite_raw`'s `open_disables_connection_recycling`) because doltlite's
/// own session state is per-connection, so one call covers the pool's life.
///
/// [`Pin::Unpinned`] builds the same views over the bare tables, so queries
/// keep working against a store with nothing committed — but they then read
/// the working set, which is only safe while nothing else writes the file. It
/// warns when it does that, because a fallback which succeeds quietly is how
/// you end up reading torn rows and never finding out.
pub async fn install_views(pool: &sqlx::SqlitePool, pin: &Pin) -> Result<usize> {
    // `sqlite_*` are the engine's own bookkeeping tables; SQLite refuses to
    // create a view over some of them, and a reader has no business in them.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await?;
    let tables: Vec<&String> = names.iter().filter(|t| is_table_name(t)).collect();

    let Pin::At(commit) = pin else {
        tracing::warn!(
            tables = tables.len(),
            "install_views: no commit to pin to, so the views read the working set. \
             Safe only while nothing else writes this store."
        );
        for t in &tables {
            create_view(pool, t, &format!("SELECT * FROM main.{t}")).await?;
        }
        return Ok(tables.len());
    };

    let modules: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_module_list WHERE name LIKE 'dolt_at_%'")
            .fetch_all(pool)
            .await?;
    let pinnable: std::collections::HashSet<&str> = modules
        .iter()
        .filter_map(|m| m.strip_prefix("dolt_at_"))
        .collect();

    for t in &tables {
        // A table with no `dolt_at_` module did not exist at this commit, so
        // its pinned contents are empty. `WHERE 0` keeps the view's columns
        // while returning nothing, which is the honest answer: there is no
        // committed state, and the uncommitted rows are not ours to read.
        let body = if pinnable.contains(t.as_str()) {
            format!("SELECT * FROM dolt_at_{t}('{commit}')")
        } else {
            format!("SELECT * FROM main.{t} WHERE 0")
        };
        create_view(pool, t, &body).await?;
    }
    Ok(tables.len())
}

async fn create_view(pool: &sqlx::SqlitePool, table: &str, body: &str) -> Result<()> {
    // Audited: `table` passed `is_table_name`, and `body` was built by the
    // caller from a static template plus that same name and a hash `Pin::at`
    // validated as 40 hex characters. Nothing here came from upstream data.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TEMP VIEW IF NOT EXISTS {VIEW_PREFIX}{table} AS {body}"
    )))
    .execute(pool)
    .await?;
    Ok(())
}

fn is_table_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "cb290c9a12e6e5c1053568c864ab582adbf42743";

    /// `HEAD` resolves when the query runs, so a pin holding it would let a
    /// consumer's diff and its content reads name different commits. The type
    /// refuses it, which is how that race is made unrepresentable rather than
    /// merely discouraged.
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
    }

    #[test]
    fn a_scan_that_named_no_commit_is_unpinned() {
        assert_eq!(Pin::from_scan(None).unwrap(), Pin::Unpinned);
        assert!(Pin::from_scan(Some(HASH)).unwrap().is_pinned());
        assert!(Pin::from_scan(Some("nonsense")).is_err());
    }
}

#[cfg(test)]
mod view_tests {
    //! Each of these guards one property the streaming design rests on, and
    //! each fails loudly if doltlite or SQLite stops behaving this way.

    use super::*;

    async fn store(dir: &std::path::Path, name: &str, ddl: &[&str]) -> sqlx::SqlitePool {
        crate::doltlite_raw::open(&dir.join(name), ddl)
            .await
            .unwrap()
    }

    /// The point of the `pinned_` prefix over shadowing the real table name: a
    /// query that runs without the views installed must fail, not silently
    /// read the working set. This is the whole safety argument for the naming,
    /// so it gets a test of its own.
    #[tokio::test]
    async fn a_missing_pinned_view_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store(
            dir.path(),
            "missing.doltlite_db",
            &["CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY)"],
        )
        .await;
        let err = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pinned_notes")
            .fetch_one(&pool)
            .await
            .expect_err("reading a view nobody installed must be an error");
        assert!(
            err.to_string().contains("no such table"),
            "expected a missing-table error, got: {err}"
        );
    }

    /// The premise of the whole plan: with the views installed, ordinary
    /// queries see committed state while the producer's working set is dirty.
    #[tokio::test]
    async fn pinned_views_read_the_commit_not_the_working_set() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store(
            dir.path(),
            "pinned.doltlite_db",
            &[
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, nick TEXT)",
                "CREATE TABLE IF NOT EXISTS msgs (id TEXT PRIMARY KEY, who TEXT)",
            ],
        )
        .await;
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return; // stock libsqlite3 dev build: no dolt_at_ to pin to.
        }
        for (t, id, other) in [("users", "u1", "ann"), ("msgs", "m1", "u1")] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {t} VALUES (?, ?)"
            )))
            .bind(id)
            .bind(other)
            .execute(&pool)
            .await
            .unwrap();
        }
        let commit = crate::doltlite_raw::commit_run(&pool, "seed")
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
        sqlx::query("INSERT INTO later VALUES ('x')")
            .execute(&pool)
            .await
            .unwrap();

        install_views(&pool, &Pin::at(&commit).unwrap())
            .await
            .unwrap();

        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count("SELECT COUNT(*) FROM users").await, 2, "working set");
        assert_eq!(
            count("SELECT COUNT(*) FROM pinned_users").await,
            1,
            "pinned"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM pinned_users u JOIN pinned_msgs m ON m.who = u.id").await,
            1,
            "a join across two pinned views is pinned on both sides"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM pinned_later").await,
            0,
            "a table that did not exist at the pin reads empty, not dirty"
        );
    }

    /// Pinned views are per-connection state. The design leans on that in two
    /// directions: installing them once when a store is opened covers every
    /// later query on that pool, and they cannot leak into anyone else's view
    /// of the same file. Our pools are size 1 with recycling disabled, so
    /// "this connection" and "this pool" are the same lifetime.
    #[tokio::test]
    async fn pinned_views_are_scoped_to_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope.doltlite_db");
        let a = crate::doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY)"],
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO notes VALUES (1)")
            .execute(&a)
            .await
            .unwrap();
        install_views(&a, &Pin::Unpinned).await.unwrap();

        for _ in 0..3 {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_notes")
                .fetch_one(&a)
                .await
                .unwrap();
            assert_eq!(n, 1, "every later query on this pool sees the views");
        }
        a.close().await;

        let b = crate::doltlite_raw::open(&path, &[]).await.unwrap();
        let err = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pinned_notes")
            .fetch_one(&b)
            .await
            .expect_err("a second connection must not inherit the pinned views");
        assert!(
            err.to_string().contains("no such table"),
            "expected a missing-table error, got: {err}"
        );
        b.close().await;
    }
}
