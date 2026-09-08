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

/// The commit a store is read at: a full hash, and nothing else.
///
/// Two states this deliberately cannot hold, because each is a way to end up
/// reading rows nobody committed.
///
/// **Not `HEAD`.** It resolves when the query runs rather than when the pin
/// was taken, so a pin carrying it would let one pass's diff and its content
/// reads name two different commits — the exact race streaming introduces.
///
/// **Not "no pin".** A store with nothing committed has nothing to read, so
/// there is no such thing as pinning to it. Callers that find no commit must
/// decide what to do — a consumer should do nothing that pass and wait — and
/// having no variant for it is what stops that decision from being made by
/// accident, silently, in favour of the working set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin(String);

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
        Ok(Pin(commit))
    }

    /// The pin a scan produced. `None` out means the scan named no commit —
    /// no commits yet, or no dolt extensions — and is the caller's to handle.
    pub fn from_scan(commit: Option<&str>) -> Result<Option<Pin>> {
        commit.map(Pin::at).transpose()
    }

    pub fn commit(&self) -> &str {
        &self.0
    }
}

/// Whose store a read is against.
///
/// The helpers that build a query around a table name — `load_payloads` and
/// friends — are used from both sides of a store: the download step reads the
/// store it is writing, and render reads somebody else's. Those want opposite
/// things. The owner wants its own working set, which is the whole point of
/// having one. Everyone else must read committed state, or they see rows the
/// writer has not finished with.
///
/// A regex cannot tell those apart at the call site, and a default would pick
/// one of them silently. So the parameter is mandatory: every caller answers
/// the question, and `Own` is greppable when you want to audit the answers.
#[derive(Debug, Clone, Copy)]
pub enum Reads<'a> {
    /// This process owns the store and is reading what it wrote.
    Own,
    /// Somebody else owns it: read at `pin`, through the views
    /// [`install_views`] created.
    At(&'a Pin),
}

impl Reads<'_> {
    /// The name this read should use for `table`.
    pub fn table(&self, table: &str) -> String {
        match self {
            Reads::Own => table.to_string(),
            Reads::At(_) => format!("{VIEW_PREFIX}{table}"),
        }
    }
}

/// The commit this store is at now, or `None` when it has no commits.
///
/// For a consumer driven by [`crate::doltlite_raw::scan_buckets`], prefer the
/// `new_head` that scan already returned: the diff and the reads that follow
/// it must name one commit, and sampling HEAD a second time can pick up a
/// commit the diff did not see. This is for the consumers that do no diff at
/// all, and for a sibling store (a blob CAS) with a HEAD of its own.
pub async fn head(pool: &sqlx::SqlitePool) -> Result<Option<Pin>> {
    // `dolt_hashof` resolves the ref, rather than ordering `dolt_log()` by a
    // `date` that only has second resolution -- checkpointing commits several
    // times a second, so ties are the normal case, not the edge one.
    let commit: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
        .fetch_optional(pool)
        .await
        // No `dolt_hashof` at all is a build without the extensions, which
        // reads the same as a store with nothing committed: no pin.
        .unwrap_or(None);
    if commit.is_none() {
        // Every caller turns this into "nothing to read". The render paths
        // now skip on it rather than reporting a completed pass over zero
        // rows, but this line is still the only place the difference between
        // "absent" and "empty" is stated rather than inferred.
        tracing::warn!(
            store = %store_filename(pool),
            "no commit to pin: reading this store as empty, which is not the \
             same as it being empty",
        );
        return Ok(None);
    }
    // The same answer reached the other way. A doltlite file is born with an
    // initialization commit, so HEAD resolves even for a store whose tables
    // have never been committed -- and there every pinned view would be the
    // empty one, which reads as a source that lost all its rows.
    if !carries_committed_schema(pool).await {
        tracing::warn!(
            store = %store_filename(pool),
            "tables exist but no commit carries them: unreadable, not empty",
        );
        return Ok(None);
    }
    Pin::from_scan(commit.as_deref())
}

/// The file a pool is against, for a log line.
fn store_filename(pool: &sqlx::SqlitePool) -> String {
    pool.connect_options().get_filename().display().to_string()
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
/// There is deliberately no "install them unpinned" path. A caller with no
/// commit to pin to is already in trouble — the store has nothing committed —
/// and building views over the bare tables would answer that by handing back
/// the working set, which is the failure this module exists to prevent.
/// Whether any commit in this store carries a schema.
///
/// A doltlite file gets an "Initialize data repository" commit when it is
/// created, before any DDL — so `dolt_hashof('HEAD')` answers for a store
/// whose tables have never been committed at all. That is the dangerous
/// shape, because it does not look like an error anywhere:
///
/// - `head` returns a real hash, so the store reads as pinnable;
/// - `install_views` finds the tables in `sqlite_master` but no `dolt_at_`
///   module for them, so every view becomes the empty `WHERE 0` one;
/// - the consumer reads zero rows and reports a *completed* walk;
/// - and `retain_documents` deletes every document the source had.
///
/// It is reachable: a download that created its tables and wrote rows, then
/// died before its first commit, leaves exactly this.
///
/// A store that has committed anything has a `dolt_at_<table>` module per
/// committed table, so their total absence *while tables exist* is the
/// signal. A store with no tables at all needs no answer here — nothing
/// creates a view, and the read fails loudly on its own.
pub(crate) async fn carries_committed_schema(pool: &sqlx::SqlitePool) -> bool {
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if tables == 0 {
        return true;
    }
    let modules: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pragma_module_list WHERE name LIKE 'dolt_at_%'")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    modules > 0
}

pub async fn install_views(pool: &sqlx::SqlitePool, pin: &Pin) -> Result<usize> {
    warn_if_dirty(pool).await;
    // `sqlite_*` are the engine's own bookkeeping tables; SQLite refuses to
    // create a view over some of them, and a reader has no business in them.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await?;
    let tables: Vec<&String> = names.iter().filter(|t| is_table_name(t)).collect();
    let commit = pin.commit();

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
        // while returning nothing.
        //
        // That is only honest because `head` has already refused the store
        // where *nothing* is committed. Here some other table is committed,
        // so this one is genuinely newer than the pin and genuinely empty at
        // it. Without that check the same branch would quietly turn "this
        // store has never been committed" into "this source has no rows",
        // which is what makes a consumer delete everything.
        let body = if pinnable.contains(t.as_str()) {
            format!("SELECT * FROM dolt_at_{t}('{commit}')")
        } else {
            format!("SELECT * FROM main.{t} WHERE 0")
        };
        create_view(pool, t, &body).await?;
    }
    Ok(tables.len())
}

/// `None` from a build with no dolt extensions, where there is no
/// `dolt_status` to ask.
async fn dirty_table_count(pool: &sqlx::SqlitePool) -> Option<i64> {
    sqlx::query_scalar("SELECT count(*) FROM dolt_status")
        .fetch_one(pool)
        .await
        .ok()
}

/// The reader's half of what `doltlite_raw`'s rescue commit does for a writer.
///
/// A writer seals a dirty tree on the way in. A reader must not write to a
/// store it does not own, so all it can do is say what it is about to miss:
/// rows nobody committed are invisible to every pinned read below, and they
/// read back exactly like a source that holds nothing. That ambiguity is the
/// whole reason this line exists — nothing downstream can recover it.
async fn warn_if_dirty(pool: &sqlx::SqlitePool) {
    let Some(dirty) = dirty_table_count(pool).await else {
        return;
    };
    if dirty > 0 {
        tracing::warn!(
            store = %store_filename(pool),
            dirty_tables = dirty,
            "reading a store with uncommitted changes: whoever wrote them never \
             sealed, and a pinned read cannot tell those rows from an empty source",
        );
    }
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

    /// A scan that named no commit hands back `None`, not a pin that reads
    /// the working set. The caller has to say what to do about it, which is
    /// the point: for a streaming consumer the answer is "do nothing this
    /// pass", and that must never be reached by default.
    #[test]
    fn a_scan_that_named_no_commit_has_no_pin() {
        assert_eq!(Pin::from_scan(None).unwrap(), None);
        assert_eq!(Pin::from_scan(Some(HASH)).unwrap().unwrap().commit(), HASH);
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

    /// The one thing a reader can say about the store this whole file is
    /// careful not to read: it is dirty, so somebody wrote rows and never
    /// sealed them. A pinned read cannot tell those rows from a source that
    /// holds nothing, which is how three live tests came to download a page
    /// and then assert on zero rows.
    #[tokio::test]
    async fn a_writer_that_never_sealed_leaves_the_store_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(
            &dir.path().join("unsealed.doltlite_db"),
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        assert_eq!(
            dirty_table_count(&pool).await,
            Some(0),
            "a freshly opened store is sealed by the open itself"
        );

        sqlx::query("INSERT INTO entities VALUES ('never-sealed')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            dirty_table_count(&pool).await,
            Some(1),
            "a row nobody committed has to be visible as dirtiness, or the \
             reader has nothing at all to warn about"
        );

        // And the read really is blind to it: HEAD is still the schema
        // commit, which has no rows.
        let pin = head(&pool)
            .await
            .unwrap()
            .expect("the open commits a schema");
        install_views(&pool, &pin).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_entities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "the pinned read sees an empty store, not the row");

        crate::doltlite_raw::commit_run(&pool, "seal")
            .await
            .unwrap();
        assert_eq!(dirty_table_count(&pool).await, Some(0), "sealing cleans it");
    }

    /// The guarantee every pinned render now rests on, stated once against a
    /// store shaped like a provider's: a row written but not committed must
    /// not be visible through the views, and one that *was* committed must.
    ///
    /// Each provider gets this property by construction — it opens with
    /// `open_reader`, pins, installs the views, and reads `pinned_<table>`,
    /// and the repo lint refuses a render read that does not. This test is
    /// what makes that chain mean something: if `install_views` ever stopped
    /// excluding the working set, every provider would silently start
    /// rendering half-written rows and no provider test would notice, because
    /// none of them writes uncommitted data on purpose.
    #[tokio::test]
    async fn an_uncommitted_row_is_invisible_through_the_views() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(
            &dir.path().join("provider.doltlite_db"),
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query("INSERT INTO entities VALUES ('committed', 'a')")
            .execute(&pool)
            .await
            .unwrap();
        let commit = crate::doltlite_raw::commit_run(&pool, "one entity")
            .await
            .unwrap()
            .unwrap();
        // The shape a download mid-run leaves behind.
        sqlx::query("INSERT INTO entities VALUES ('in-flight', 'b')")
            .execute(&pool)
            .await
            .unwrap();

        install_views(&pool, &Pin::at(&commit).unwrap())
            .await
            .unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM pinned_entities ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            ids,
            vec!["committed".to_string()],
            "the pinned view must show the committed row and not the in-flight one"
        );
    }

    /// `dolt_log().date` has one-second resolution, so a burst of commits --
    /// which is what checkpointing produces -- gives several rows one
    /// timestamp. Ordering by it leaves the winner to an unspecified tiebreak;
    /// `head` must name the actual HEAD every time.
    #[tokio::test]
    async fn head_tracks_head_through_a_burst_of_same_second_commits() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(
            &dir.path().join("burst.doltlite_db"),
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }

        for i in 0..8 {
            sqlx::query("INSERT INTO entities VALUES (?, 'x')")
                .bind(format!("e{i}"))
                .execute(&pool)
                .await
                .unwrap();
            let sealed = crate::doltlite_raw::commit_run(&pool, &format!("batch {i}"))
                .await
                .unwrap()
                .unwrap();

            let pin = head(&pool)
                .await
                .unwrap()
                .expect("a sealed store has a head");
            assert_eq!(
                pin.commit(),
                sealed,
                "head must name the commit just sealed, not a same-second sibling"
            );

            // And the pin has to carry every row committed so far -- naming the
            // right hash is only half of it.
            // Safe: `pin.commit()` is validated hex, and the table name is a
            // literal -- the same argument `install_views` makes.
            let seen: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM dolt_at_entities('{}')",
                pin.commit()
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(seen, i64::from(i) + 1, "pin at batch {i} lost rows");
        }
    }

    /// The shape that deletes a source: tables written, nothing committed.
    ///
    /// A doltlite file has an initialization commit from birth, so
    /// `dolt_hashof('HEAD')` answers even here — and every `dolt_at_` module
    /// is absent, so `install_views` would give each table the empty
    /// `WHERE 0` view. The consumer then reads zero rows, calls that a
    /// completed walk, and sweeps every document the source had.
    ///
    /// `head` must say `None` — "I cannot read this" — rather than hand back
    /// a pin that reads as an empty source.
    #[tokio::test]
    async fn a_store_whose_tables_were_never_committed_is_not_readable() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately not `doltlite_raw::open`: that commits the schema on
        // the way in, which is the guarantee. This reproduces a download
        // that created its tables and died before its first commit.
        let path = dir.path().join("half.doltlite_db");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
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

        // The store does have a commit -- the one it was born with.
        let born_with: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
            .fetch_optional(&pool)
            .await
            .unwrap_or(None);
        assert!(
            born_with.is_some(),
            "precondition: doltlite gives a new file an initialization commit, \
             which is why this case cannot be caught by asking for a hash"
        );

        assert!(
            head(&pool).await.unwrap().is_none(),
            "a store whose schema has never been committed must read as \
             unreadable, not as a source that lost all its rows"
        );
    }

    /// The other half: a store that *has* committed its schema and holds no
    /// rows is readable and empty. A consumer may act on that — including
    /// deleting what the source no longer has.
    #[tokio::test]
    async fn a_committed_but_empty_store_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::doltlite_raw::open(
            &dir.path().join("empty.doltlite_db"),
            &["CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }

        let pin = head(&pool)
            .await
            .unwrap()
            .expect("open commits the schema, so the store is readable");
        install_views(&pool, &pin).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pinned_entities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "readable and empty, which is a different answer");
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
        if !crate::doltlite_raw::has_dolt_extensions(&a).await {
            return;
        }
        sqlx::query("INSERT INTO notes VALUES (1)")
            .execute(&a)
            .await
            .unwrap();
        let commit = crate::doltlite_raw::commit_run(&a, "one note")
            .await
            .unwrap()
            .unwrap();
        install_views(&a, &Pin::at(&commit).unwrap()).await.unwrap();

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
