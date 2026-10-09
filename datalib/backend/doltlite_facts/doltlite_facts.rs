//! The doltlite behaviour datalib depends on, one test per fact, grouped
//! by the section of `docs/dev/doltlite.md` that states it. A doltlite bump
//! that moves a fact fails here by name: fix the fact in doltlite.md, then
//! the test, then whatever in the tree leaned on it. Facts about a writer
//! and a reader in two processes live in
//! `//datalib/backend/etl:doltlite_two_process_test` instead.
//!
//! Every store is a fresh tempdir file, and every connection is a single
//! sqlx connection, so the library under test is the one the pipeline
//! links. The shell's own behaviour is checked through the pinned CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};

// ── plumbing ────────────────────────────────────────────────────────

struct Store {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Store {
    fn new() -> Store {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.doltlite_db");
        Store { _dir: dir, path }
    }

    fn at(&self, rev: &str) -> String {
        format!("{}@{rev}", self.path.display())
    }

    fn size(&self) -> u64 {
        std::fs::metadata(&self.path).expect("stat store").len()
    }

    async fn rw(&self) -> SqliteConnection {
        connect(
            &self.path.display().to_string(),
            false,
            Duration::from_secs(5),
        )
        .await
    }

    async fn ro(&self) -> SqliteConnection {
        connect(
            &self.path.display().to_string(),
            true,
            Duration::from_secs(5),
        )
        .await
    }
}

async fn connect(filename: &str, read_only: bool, busy: Duration) -> SqliteConnection {
    try_connect(filename, read_only, busy)
        .await
        .unwrap_or_else(|e| panic!("open {filename}: {e}"))
}

async fn try_connect(
    filename: &str,
    read_only: bool,
    busy: Duration,
) -> Result<SqliteConnection, String> {
    let opts = SqliteConnectOptions::new()
        .filename(filename)
        .create_if_missing(!read_only)
        .read_only(read_only)
        .busy_timeout(busy);
    SqliteConnection::connect_with(&opts)
        .await
        .map_err(|e| e.to_string())
}

// Every statement below is written by this file with literal values, so
// splicing it is safe.
async fn exec(c: &mut SqliteConnection, sql: &str) -> Result<(), String> {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *c)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn ok(c: &mut SqliteConnection, sql: &str) {
    exec(c, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn text(c: &mut SqliteConnection, sql: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(&mut *c)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn int(c: &mut SqliteConnection, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(&mut *c)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn texts(c: &mut SqliteConnection, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_all(&mut *c)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    v.sort();
    v
}

async fn plan(c: &mut SqliteConnection, sql: &str) -> String {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    sqlx::query(sqlx::AssertSqlSafe(explain))
        .fetch_all(&mut *c)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn commit(c: &mut SqliteConnection, msg: &str) -> String {
    text(c, &format!("SELECT dolt_commit('-Am', '{msg}')"))
        .await
        .expect("dolt_commit returns the new hash")
}

async fn head(c: &mut SqliteConnection) -> String {
    text(c, "SELECT commit_hash FROM dolt_log() LIMIT 1")
        .await
        .expect("a head commit")
}

fn err_contains(result: Result<(), String>, needle: &str) {
    match result {
        Ok(()) => panic!("expected an error containing {needle:?}, got success"),
        Err(e) => assert!(e.contains(needle), "expected {needle:?} in: {e}"),
    }
}

/// A store holding `t(id TEXT PRIMARY KEY, v INTEGER)` with rows `a`, `b`
/// committed once.
async fn store_with_rows() -> (Store, String) {
    let s = Store::new();
    let mut c = s.rw().await;
    ok(&mut c, "CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER)").await;
    ok(&mut c, "INSERT INTO t VALUES ('a', 1), ('b', 2)").await;
    let hash = commit(&mut c, "rows").await;
    c.close().await.expect("close");
    (s, hash)
}

fn cli() -> PathBuf {
    PathBuf::from(std::env::var("DOLTLITE_BIN").expect("DOLTLITE_BIN is set by the BUILD rule"))
        .canonicalize()
        .expect("the doltlite CLI is in runfiles")
}

fn shell(args: &[&str]) -> (bool, String) {
    let out = Command::new(cli())
        .args(args)
        .output()
        .expect("run doltlite");
    let mut both = String::from_utf8_lossy(&out.stdout).into_owned();
    both.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), both.trim().to_string())
}

fn sidecar_lock(store: &Path) -> PathBuf {
    let name = store.file_name().unwrap().to_string_lossy();
    store.with_file_name(format!(".{name}-lock"))
}

// ── Branches, HEAD and the working set ──────────────────────────────

mod branches {
    use super::*;

    #[tokio::test]
    async fn a_fresh_connection_lands_on_main() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_checkout('-b', 'w')").await;
        c.close().await.unwrap();
        let mut again = s.rw().await;
        assert_eq!(
            text(&mut again, "SELECT active_branch()").await.as_deref(),
            Some("main")
        );
    }

    #[tokio::test]
    async fn the_working_set_belongs_to_the_branch() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "SELECT dolt_branch('w')").await;
        ok(&mut w, "SELECT dolt_connect_branch('w')").await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;

        let mut on_main = s.rw().await;
        assert_eq!(int(&mut on_main, "SELECT COUNT(*) FROM t").await, 2);
        assert_eq!(
            int(&mut on_main, "SELECT COUNT(*) FROM dolt_status").await,
            0
        );

        let mut also_on_w = s.rw().await;
        ok(&mut also_on_w, "SELECT dolt_connect_branch('w')").await;
        assert_eq!(
            int(&mut also_on_w, "SELECT COUNT(*) FROM t").await,
            3,
            "shared across connections on w"
        );
    }

    #[tokio::test]
    async fn connect_branch_writes_nothing_and_checkout_does() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_branch('w')").await;
        c.close().await.unwrap();
        let before = s.size();

        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_connect_branch('w')").await;
        c.close().await.unwrap();
        assert_eq!(s.size(), before, "dolt_connect_branch grew the file");

        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_checkout('w')").await;
        c.close().await.unwrap();
        assert!(
            s.size() > before,
            "dolt_checkout no longer writes a working set"
        );
    }

    #[tokio::test]
    async fn a_missing_branch_is_an_error() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        err_contains(
            exec(&mut c, "SELECT dolt_connect_branch('nope')").await,
            "branch not found",
        );
        err_contains(
            exec(&mut c, "SELECT dolt_checkout('nope')").await,
            "no such branch or table",
        );
    }

    #[tokio::test]
    async fn checkout_refuses_a_commit() {
        let (s, hash) = store_with_rows().await;
        let mut c = s.rw().await;
        err_contains(
            exec(&mut c, &format!("SELECT dolt_checkout('{hash}')")).await,
            "detached head",
        );
    }

    #[tokio::test]
    async fn the_procedures_are_functions_not_call() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        assert!(exec(&mut c, "CALL DOLT_CHECKOUT('main')").await.is_err());
    }
}

// ── Locks and writers ───────────────────────────────────────────────

mod locks {
    use super::*;

    #[tokio::test]
    async fn a_second_writer_waits_then_is_locked_out() {
        let (s, _) = store_with_rows().await;
        let mut first = s.rw().await;
        ok(&mut first, "BEGIN").await;
        ok(&mut first, "INSERT INTO t VALUES ('c', 3)").await;

        let name = s.path.display().to_string();
        let mut second = connect(&name, false, Duration::from_millis(50)).await;
        err_contains(
            exec(&mut second, "INSERT INTO t VALUES ('d', 4)").await,
            "database is locked",
        );
        err_contains(exec(&mut second, "SELECT dolt_branch('x')").await, "locked");

        ok(&mut first, "COMMIT").await;
        ok(&mut second, "INSERT INTO t VALUES ('d', 4)").await;
    }

    #[tokio::test]
    async fn the_lock_is_a_sidecar_file_beside_the_store() {
        let (s, _) = store_with_rows().await;
        assert!(
            sidecar_lock(&s.path).exists(),
            "no {}",
            sidecar_lock(&s.path).display()
        );
    }
}

// ── What a read-only connection may do ──────────────────────────────

mod read_only {
    use super::*;

    #[tokio::test]
    async fn it_refuses_every_write_including_a_branch_switch() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_branch('w')").await;
        c.close().await.unwrap();

        let mut r = s.ro().await;
        let readonly = "attempt to write a readonly database";
        err_contains(
            exec(&mut r, "INSERT INTO t VALUES ('c', 3)").await,
            readonly,
        );
        err_contains(
            exec(&mut r, "SELECT dolt_connect_branch('w')").await,
            readonly,
        );
        err_contains(exec(&mut r, "SELECT dolt_checkout('w')").await, readonly);
    }

    #[tokio::test]
    async fn a_plain_select_reads_the_branch_working_set() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        let mut r = s.ro().await;
        assert_eq!(int(&mut r, "SELECT COUNT(*) FROM t").await, 3);
        assert_eq!(
            int(&mut r, "SELECT COUNT(*) FROM dolt_at_t('HEAD')").await,
            2
        );
    }

    #[tokio::test]
    async fn a_scalar_answers_from_the_last_view_until_a_table_read() {
        let (s, first) = store_with_rows().await;
        let mut r = s.ro().await;
        assert_eq!(
            text(&mut r, "SELECT dolt_hashof('HEAD')").await.as_deref(),
            Some(first.as_str())
        );

        let mut w = s.rw().await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        let second = commit(&mut w, "more").await;

        assert_eq!(
            text(&mut r, "SELECT dolt_hashof('HEAD')").await.as_deref(),
            Some(first.as_str())
        );
        int(&mut r, "SELECT COUNT(*) FROM sqlite_master").await;
        assert_eq!(
            text(&mut r, "SELECT dolt_hashof('HEAD')").await.as_deref(),
            Some(second.as_str())
        );
    }

    #[tokio::test]
    async fn per_table_modules_are_registered_on_first_use() {
        let (s, _) = store_with_rows().await;
        let mut r = s.ro().await;
        let listed = "SELECT COUNT(*) FROM pragma_module_list WHERE name = 'dolt_at_t'";
        assert_eq!(
            int(&mut r, listed).await,
            0,
            "dolt_at_t is listed before any use"
        );

        let mut w = s.rw().await;
        ok(&mut w, "CREATE TABLE later (id TEXT PRIMARY KEY)").await;
        ok(&mut w, "INSERT INTO later VALUES ('x')").await;
        commit(&mut w, "later").await;
        int(&mut r, "SELECT COUNT(*) FROM sqlite_master").await;
        assert_eq!(
            int(&mut r, "SELECT COUNT(*) FROM dolt_at_later('HEAD')").await,
            1,
            "a table committed after the reader opened reads without a reopen"
        );
    }

    #[tokio::test]
    async fn a_per_table_module_is_named_as_its_table_is() {
        let s = Store::new();
        let mut w = s.rw().await;
        ok(&mut w, "CREATE TABLE Mixed_Case (id TEXT PRIMARY KEY)").await;
        ok(
            &mut w,
            "CREATE TABLE \"odd \"\"name\"\"\" (id TEXT PRIMARY KEY)",
        )
        .await;
        ok(&mut w, "INSERT INTO Mixed_Case VALUES ('x'), ('y')").await;
        ok(&mut w, "INSERT INTO \"odd \"\"name\"\"\" VALUES ('x')").await;
        commit(&mut w, "load").await;
        w.close().await.unwrap();

        let mut r = s.ro().await;
        for sql in [
            "SELECT COUNT(*) FROM dolt_at_Mixed_Case('HEAD')",
            "SELECT COUNT(*) FROM dolt_at_mixed_case('HEAD')",
            "SELECT COUNT(*) FROM \"dolt_at_Mixed_Case\"('HEAD')",
        ] {
            assert_eq!(int(&mut r, sql).await, 2, "{sql}");
        }
        assert_eq!(
            int(
                &mut r,
                "SELECT COUNT(*) FROM \"dolt_at_odd \"\"name\"\"\"('HEAD')"
            )
            .await,
            1,
            "a quoted module name reaches a table no bare identifier can name"
        );
        err_contains(
            exec(&mut r, "SELECT COUNT(*) FROM \"dolt_at_Absent\"('HEAD')").await,
            "no such table: dolt_at_Absent",
        );
    }

    #[tokio::test]
    async fn a_held_read_transaction_is_one_commit() {
        let (s, _) = store_with_rows().await;
        let mut r = s.ro().await;
        ok(&mut r, "BEGIN").await;
        assert_eq!(int(&mut r, "SELECT COUNT(*) FROM t").await, 2);

        let mut w = s.rw().await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        commit(&mut w, "more").await;

        assert_eq!(int(&mut r, "SELECT COUNT(*) FROM t").await, 2);
        ok(&mut r, "COMMIT").await;
        assert_eq!(int(&mut r, "SELECT COUNT(*) FROM t").await, 3);
    }

    #[tokio::test]
    async fn a_read_only_open_never_creates_the_file() {
        let s = Store::new();
        let name = s.path.display().to_string();
        assert!(try_connect(&name, true, Duration::from_secs(1))
            .await
            .is_err());
        assert!(!s.path.exists());
    }
}

// ── Opening a revision by path ──────────────────────────────────────

mod revision_by_path {
    use super::*;

    #[tokio::test]
    async fn a_commit_opens_detached_read_only_and_indexed() {
        let (s, first) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        commit(&mut w, "more").await;

        let mut d = connect(&s.at(&first), true, Duration::from_secs(5)).await;
        assert_eq!(text(&mut d, "SELECT active_branch()").await, None);
        assert_eq!(int(&mut d, "SELECT COUNT(*) FROM t").await, 2);
        assert_eq!(
            text(&mut d, "SELECT dolt_hashof('HEAD')").await.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(int(&mut d, "SELECT COUNT(*) FROM dolt_log()").await, 2);
        err_contains(
            exec(&mut d, "INSERT INTO t VALUES ('z', 9)").await,
            "attempt to write a readonly database",
        );
        let p = plan(&mut d, "SELECT * FROM t WHERE id = 'a'").await;
        assert!(
            p.contains("SEARCH t USING"),
            "a detached open did not use the key: {p}"
        );
    }

    #[tokio::test]
    async fn a_detached_open_stays_put_when_the_branch_moves() {
        let (s, tip) = store_with_rows().await;
        let mut d = connect(&s.at(&tip), true, Duration::from_secs(5)).await;
        let mut w = s.rw().await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        commit(&mut w, "more").await;
        assert_eq!(int(&mut d, "SELECT COUNT(*) FROM t").await, 2);
    }

    #[tokio::test]
    async fn a_tag_and_an_ancestor_open_detached_too() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "SELECT dolt_tag('v1')").await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        commit(&mut w, "more").await;
        for rev in ["v1", "main~1"] {
            let mut d = connect(&s.at(rev), true, Duration::from_secs(5)).await;
            assert_eq!(text(&mut d, "SELECT active_branch()").await, None, "{rev}");
            assert_eq!(int(&mut d, "SELECT COUNT(*) FROM t").await, 2, "{rev}");
        }
    }

    /// Every reader opens this way, and reader queries lean on `IN (…)`,
    /// `IN (SELECT value FROM json_each(?))` and `DISTINCT`. Through 0.50.13
    /// a detached open refused all three as writes (dolthub/doltlite#3392).
    #[tokio::test]
    async fn a_detached_open_runs_queries_that_need_an_ephemeral_table() {
        let (s, commit) = store_with_rows().await;
        let mut d = connect(&s.at(&commit), true, Duration::from_secs(5)).await;
        assert_eq!(
            int(&mut d, "SELECT COUNT(*) FROM t WHERE id IN ('a', 'b')").await,
            2
        );
        assert_eq!(
            int(
                &mut d,
                "SELECT COUNT(*) FROM t WHERE id IN (SELECT value FROM json_each('[\"a\"]'))"
            )
            .await,
            1
        );
        assert_eq!(
            texts(&mut d, "SELECT DISTINCT CAST(v AS TEXT) FROM t").await,
            vec!["1", "2"]
        );
        assert_eq!(int(&mut d, "SELECT COUNT(DISTINCT v) FROM t").await, 2);
    }

    #[tokio::test]
    async fn a_branch_opens_read_only_by_path() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "SELECT dolt_branch('w')").await;
        let mut r = connect(&s.at("w"), true, Duration::from_secs(5)).await;
        assert_eq!(
            text(&mut r, "SELECT active_branch()").await.as_deref(),
            Some("w")
        );
    }

    #[tokio::test]
    async fn a_missing_revision_fails_the_open() {
        let (s, _) = store_with_rows().await;
        let e = try_connect(&s.at("nope"), true, Duration::from_secs(1))
            .await
            .expect_err("opened a revision that does not exist");
        assert!(e.contains("not found"), "{e}");
    }

    /// The shell switches branches itself after it opens, so under
    /// `-readonly` its `@` spelling fails and `/` is the one that works.
    #[tokio::test]
    async fn the_shell_opens_a_revision_with_a_slash_not_an_at() {
        let (s, first) = store_with_rows().await;
        let store = s.path.display().to_string();
        let sql = "SELECT quote(active_branch()) || '|' || COUNT(*) FROM t";
        let (slash_ok, slash) = shell(&["-readonly", &format!("{store}/{first}"), sql]);
        assert!(slash_ok, "{slash}");
        assert_eq!(slash, "NULL|2");
        let (at_ok, at) = shell(&["-readonly", &format!("{store}@{first}"), sql]);
        assert!(!at_ok, "the shell now accepts @ under -readonly: {at}");
    }
}

// ── Diffs ───────────────────────────────────────────────────────────

mod diffs {
    use super::*;

    /// Four commits on `t`: a and b added; a modified; c added.
    async fn history() -> (Store, [String; 3]) {
        let (s, c1) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "UPDATE t SET v = 10 WHERE id = 'a'").await;
        let c2 = commit(&mut w, "modify a").await;
        ok(&mut w, "INSERT INTO t VALUES ('c', 3)").await;
        let c3 = commit(&mut w, "add c").await;
        (s, [c1, c2, c3])
    }

    const ROW: &str = "coalesce(to_id, from_id) || ':' || diff_type";

    #[tokio::test]
    async fn refs_as_arguments_or_either_pair_of_filters_agree() {
        let (s, [c1, _, c3]) = history().await;
        let mut r = s.ro().await;
        let args = texts(
            &mut r,
            &format!("SELECT {ROW} FROM dolt_diff_t('{c1}', '{c3}')"),
        )
        .await;
        assert_eq!(args, vec!["a:modified", "c:added"]);
        let by_ref =
            format!("SELECT {ROW} FROM dolt_diff_t WHERE from_ref = '{c1}' AND to_ref = '{c3}'");
        assert_eq!(texts(&mut r, &by_ref).await, args);
        let by_commit = format!(
            "SELECT {ROW} FROM dolt_diff_t WHERE from_commit = '{c1}' AND to_commit = '{c3}'"
        );
        assert_eq!(texts(&mut r, &by_commit).await, args);
    }

    #[tokio::test]
    async fn a_range_is_one_comparison() {
        let (s, [c1, _, c3]) = history().await;
        let mut r = s.ro().await;
        let range = format!("FROM dolt_diff_t WHERE from_ref = '{c1}..{c3}'");
        assert_eq!(
            texts(&mut r, &format!("SELECT {ROW} {range}")).await,
            vec!["a:modified", "c:added"]
        );
        assert_eq!(
            texts(&mut r, &format!("SELECT DISTINCT to_commit {range}")).await,
            vec![c3]
        );
    }

    #[tokio::test]
    async fn no_refs_walks_every_adjacent_pair() {
        let (s, _) = history().await;
        let mut r = s.ro().await;
        assert_eq!(
            texts(&mut r, &format!("SELECT {ROW} FROM dolt_diff_t")).await,
            vec!["a:added", "a:modified", "b:added", "c:added"]
        );
    }

    #[tokio::test]
    async fn diff_type_is_added_modified_or_removed() {
        let (s, _) = history().await;
        let mut w = s.rw().await;
        ok(&mut w, "DELETE FROM t WHERE id = 'b'").await;
        commit(&mut w, "remove b").await;
        let kinds = texts(&mut w, "SELECT DISTINCT diff_type FROM dolt_diff_t").await;
        assert_eq!(kinds, vec!["added", "modified", "removed"]);
    }

    #[tokio::test]
    async fn an_unchanged_row_is_no_change() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "INSERT OR REPLACE INTO t VALUES ('a', 1), ('b', 2)").await;
        ok(&mut w, "DELETE FROM t WHERE id = 'b'").await;
        ok(&mut w, "INSERT INTO t VALUES ('b', 2)").await;
        assert_eq!(int(&mut w, "SELECT COUNT(*) FROM dolt_status").await, 0);
        err_contains(
            exec(&mut w, "SELECT dolt_commit('-Am', 'nothing')").await,
            "nothing to commit",
        );
    }

    /// The SQLite mirrors drop and refill a keyless table; the diff pairs
    /// rows by rowid, so one row gone reads as a run of `modified`.
    #[tokio::test]
    async fn a_keyless_table_diffs_by_rowid() {
        let s = Store::new();
        let mut w = s.rw().await;
        ok(&mut w, "CREATE TABLE k (name TEXT)").await;
        ok(
            &mut w,
            "INSERT INTO k VALUES ('alpha'), ('beta'), ('delta'), ('epsilon')",
        )
        .await;
        commit(&mut w, "one").await;
        ok(&mut w, "DROP TABLE k").await;
        ok(&mut w, "CREATE TABLE k (name TEXT)").await;
        ok(
            &mut w,
            "INSERT INTO k VALUES ('alpha'), ('delta'), ('epsilon'), ('gamma')",
        )
        .await;
        commit(&mut w, "two").await;
        let rows = texts(
            &mut w,
            "SELECT diff_type || ':' || from_name || '>' || to_name FROM dolt_diff_k('HEAD~1', 'HEAD')",
        )
        .await;
        assert_eq!(
            rows,
            vec![
                "modified:beta>delta",
                "modified:delta>epsilon",
                "modified:epsilon>gamma"
            ]
        );
    }

    #[tokio::test]
    async fn history_seeks_a_text_primary_key() {
        let (s, _) = history().await;
        let mut r = s.ro().await;
        let by_key = plan(&mut r, "SELECT * FROM dolt_history_t WHERE id = 'a'").await;
        let by_value = plan(&mut r, "SELECT * FROM dolt_history_t WHERE v = 1").await;
        assert_ne!(
            by_key, by_value,
            "history no longer pushes the key down: {by_key}"
        );
        assert_eq!(
            int(&mut r, "SELECT COUNT(*) FROM dolt_history_t WHERE id = 'a'").await,
            3
        );
    }

    #[tokio::test]
    async fn a_diff_across_a_rename_shows_every_row_added() {
        let (s, before) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "ALTER TABLE t RENAME TO u").await;
        commit(&mut w, "rename").await;
        let rows = texts(
            &mut w,
            &format!("SELECT diff_type FROM dolt_diff_u('{before}', 'HEAD')"),
        )
        .await;
        assert_eq!(rows, vec!["added", "added"]);
    }
}

// ── Reverting a commit ──────────────────────────────────────────────

mod revert {
    use super::*;

    /// `t` with rows `a`, `b` committed, then `c` added in a commit of its
    /// own, then `d` in another: `(store, hash of "add c")`.
    async fn store_with_three_commits() -> (Store, String) {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        ok(&mut c, "INSERT INTO t VALUES ('c', 3)").await;
        let add_c = commit(&mut c, "add c").await;
        ok(&mut c, "INSERT INTO t VALUES ('d', 4)").await;
        commit(&mut c, "add d").await;
        c.close().await.unwrap();
        (s, add_c)
    }

    #[tokio::test]
    async fn a_revert_is_a_new_commit_undoing_one_that_need_not_be_head() {
        let (s, add_c) = store_with_three_commits().await;
        let mut c = s.rw().await;
        let reverted = text(&mut c, &format!("SELECT dolt_revert('{add_c}')"))
            .await
            .expect("dolt_revert returns the new hash");
        assert_eq!(head(&mut c).await, reverted);
        assert_eq!(texts(&mut c, "SELECT id FROM t").await, ["a", "b", "d"]);
        assert_eq!(
            text(&mut c, "SELECT message FROM dolt_log() LIMIT 1").await,
            Some("Revert \"add c\"".to_string())
        );
        assert_eq!(
            texts(&mut c, "SELECT table_name FROM dolt_status").await,
            Vec::<String>::new(),
            "the revert leaves nothing uncommitted"
        );
    }

    #[tokio::test]
    async fn a_revert_is_refused_when_a_later_commit_changed_the_same_row() {
        let (s, add_c) = store_with_three_commits().await;
        let mut c = s.rw().await;
        ok(&mut c, "UPDATE t SET v = 30 WHERE id = 'c'").await;
        commit(&mut c, "change c").await;
        let before = head(&mut c).await;
        err_contains(
            exec(&mut c, &format!("SELECT dolt_revert('{add_c}')")).await,
            "conflicts detected",
        );
        assert_eq!(
            head(&mut c).await,
            before,
            "a refused revert commits nothing"
        );
        assert_eq!(
            int(&mut c, "SELECT v FROM t WHERE id = 'c'").await,
            30,
            "and changes nothing"
        );
    }

    #[tokio::test]
    async fn a_revert_of_a_reverted_commit_is_nothing_to_commit_and_a_revert_of_the_revert_restores(
    ) {
        let (s, add_c) = store_with_three_commits().await;
        let mut c = s.rw().await;
        let undo = text(&mut c, &format!("SELECT dolt_revert('{add_c}')"))
            .await
            .unwrap();
        err_contains(
            exec(&mut c, &format!("SELECT dolt_revert('{add_c}')")).await,
            "nothing to commit",
        );
        ok(&mut c, &format!("SELECT dolt_revert('{undo}')")).await;
        assert_eq!(
            texts(&mut c, "SELECT id FROM t").await,
            ["a", "b", "c", "d"]
        );
    }

    #[tokio::test]
    async fn a_revert_is_refused_over_an_uncommitted_change() {
        let (s, add_c) = store_with_three_commits().await;
        let mut c = s.rw().await;
        ok(&mut c, "INSERT INTO t VALUES ('e', 5)").await;
        err_contains(
            exec(&mut c, &format!("SELECT dolt_revert('{add_c}')")).await,
            "Your local changes would be overwritten by revert",
        );
        assert_eq!(
            texts(&mut c, "SELECT id FROM t").await,
            ["a", "b", "c", "d", "e"]
        );
    }
}

// ── Drafting on a branch ────────────────────────────────────────────

mod drafts {
    use super::*;

    /// `p(id TEXT PRIMARY KEY, name TEXT, note TEXT)` holding Riker and
    /// Worf, committed, with a branch `draft` cut there:
    /// `(store, hash of the cut)`.
    async fn store_with_draft() -> (Store, String) {
        let s = Store::new();
        let mut c = s.rw().await;
        ok(
            &mut c,
            "CREATE TABLE p (id TEXT PRIMARY KEY, name TEXT, note TEXT)",
        )
        .await;
        ok(
            &mut c,
            "INSERT INTO p VALUES ('r', 'Riker', 'x'), ('w', 'Worf', 'y')",
        )
        .await;
        let cut = commit(&mut c, "base").await;
        ok(&mut c, "SELECT dolt_branch('draft')").await;
        c.close().await.unwrap();
        (s, cut)
    }

    async fn on_draft(s: &Store) -> SqliteConnection {
        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_connect_branch('draft')").await;
        c
    }

    async fn name_of(c: &mut SqliteConnection, id: &str) -> Option<String> {
        text(c, &format!("SELECT name FROM p WHERE id = '{id}'")).await
    }

    /// The draft renames Riker and commits; `main` changes `column` of
    /// Riker's row to `value` and commits.
    async fn both_edit_riker(s: &Store, column: &str, value: &str) {
        let mut d = on_draft(s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        commit(&mut d, "rename").await;
        d.close().await.unwrap();
        let mut m = s.rw().await;
        ok(
            &mut m,
            &format!("UPDATE p SET {column} = '{value}' WHERE id = 'r'"),
        )
        .await;
        commit(&mut m, "edit on main").await;
        m.close().await.unwrap();
    }

    async fn parents_of_head(c: &mut SqliteConnection) -> i64 {
        int(
            c,
            "SELECT COUNT(*) FROM dolt_commit_ancestors
              WHERE commit_hash = (SELECT commit_hash FROM dolt_log() LIMIT 1)",
        )
        .await
    }

    #[tokio::test]
    async fn an_uncommitted_write_on_a_branch_outlives_its_connection() {
        let (s, _) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        d.close().await.unwrap();

        let mut again = on_draft(&s).await;
        assert_eq!(
            name_of(&mut again, "r").await.as_deref(),
            Some("Will Riker")
        );
        assert_eq!(
            texts(&mut again, "SELECT table_name FROM dolt_status").await,
            ["p"],
            "still uncommitted"
        );
        let mut m = s.rw().await;
        assert_eq!(name_of(&mut m, "r").await.as_deref(), Some("Riker"));
    }

    #[tokio::test]
    async fn a_hard_reset_on_main_leaves_another_branchs_uncommitted_rows() {
        let (s, _) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        d.close().await.unwrap();

        let mut m = s.rw().await;
        ok(&mut m, "INSERT INTO p VALUES ('t', 'Troi', 'z')").await;
        ok(&mut m, "SELECT dolt_reset('--hard')").await;
        ok(&mut m, "SELECT dolt_clean()").await;
        m.close().await.unwrap();

        let mut d = on_draft(&s).await;
        assert_eq!(name_of(&mut d, "r").await.as_deref(), Some("Will Riker"));
    }

    #[tokio::test]
    async fn a_diff_to_working_reads_the_uncommitted_rows() {
        let (s, cut) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        let rows: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT from_name, to_name, diff_type FROM dolt_diff_p('{cut}', 'WORKING')"
        )))
        .fetch_all(&mut d)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [(
                "Riker".to_string(),
                "Will Riker".to_string(),
                "modified".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn merge_base_names_the_commit_a_branch_was_cut_from() {
        let (s, cut) = store_with_draft().await;
        both_edit_riker(&s, "note", "first officer").await;
        let mut m = s.rw().await;
        assert_eq!(
            text(&mut m, "SELECT dolt_merge_base('draft', 'main')").await,
            Some(cut)
        );
    }

    #[tokio::test]
    async fn a_merge_takes_a_branchs_commits_not_its_uncommitted_rows() {
        let (s, _) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        d.close().await.unwrap();

        let mut m = s.rw().await;
        assert_eq!(
            text(&mut m, "SELECT dolt_merge('draft')").await.as_deref(),
            Some("Already up to date")
        );
        assert_eq!(name_of(&mut m, "r").await.as_deref(), Some("Riker"));
    }

    #[tokio::test]
    async fn a_merge_into_a_branch_with_uncommitted_rows_is_refused() {
        let (s, _) = store_with_draft().await;
        both_edit_riker(&s, "note", "first officer").await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET note = 'dirty' WHERE id = 'w'").await;
        err_contains(
            exec(&mut d, "SELECT dolt_merge('main')").await,
            "uncommitted changes",
        );
    }

    #[tokio::test]
    async fn edits_to_different_columns_of_one_row_merge_cleanly() {
        let (s, _) = store_with_draft().await;
        both_edit_riker(&s, "note", "first officer").await;
        let mut m = s.rw().await;
        ok(&mut m, "SELECT dolt_merge('draft')").await;
        let row: (String, String) = sqlx::query_as("SELECT name, note FROM p WHERE id = 'r'")
            .fetch_one(&mut m)
            .await
            .unwrap();
        assert_eq!(row, ("Will Riker".into(), "first officer".into()));
        assert_eq!(parents_of_head(&mut m).await, 2, "a merge commit");
    }

    #[tokio::test]
    async fn a_merge_that_conflicts_outside_a_transaction_changes_nothing() {
        let (s, _) = store_with_draft().await;
        both_edit_riker(&s, "name", "Number One").await;
        let mut m = s.rw().await;
        let before = head(&mut m).await;
        err_contains(
            exec(&mut m, "SELECT dolt_merge('draft')").await,
            "conflicts detected",
        );
        assert_eq!(head(&mut m).await, before);
        assert_eq!(name_of(&mut m, "r").await.as_deref(), Some("Number One"));
    }

    #[tokio::test]
    async fn inside_a_transaction_a_conflict_resolves_as_theirs_and_commits() {
        let (s, _) = store_with_draft().await;
        both_edit_riker(&s, "name", "Number One").await;
        let mut m = s.rw().await;
        ok(&mut m, "BEGIN").await;
        err_contains(
            exec(&mut m, "SELECT dolt_merge('draft')").await,
            "Merge has 1 conflict(s)",
        );
        let sides: (String, String, String) =
            sqlx::query_as("SELECT base_name, our_name, their_name FROM dolt_conflicts_p")
                .fetch_one(&mut m)
                .await
                .unwrap();
        assert_eq!(
            sides,
            ("Riker".into(), "Number One".into(), "Will Riker".into())
        );
        ok(&mut m, "SELECT dolt_conflicts_resolve('--theirs', 'p')").await;
        assert_eq!(int(&mut m, "SELECT COUNT(*) FROM dolt_conflicts").await, 0);
        commit(&mut m, "save").await;
        err_contains(exec(&mut m, "COMMIT").await, "no transaction is active");

        assert_eq!(name_of(&mut m, "r").await.as_deref(), Some("Will Riker"));
        assert_eq!(parents_of_head(&mut m).await, 2, "a merge commit");
    }

    #[tokio::test]
    async fn a_squash_merge_is_one_commit_with_one_parent() {
        let (s, _) = store_with_draft().await;
        both_edit_riker(&s, "note", "first officer").await;
        let mut m = s.rw().await;
        let main_before = head(&mut m).await;
        let squashed = text(&mut m, "SELECT dolt_merge('--squash', 'draft')")
            .await
            .expect("a squash returns its commit");
        assert_eq!(head(&mut m).await, squashed, "committed, not staged");
        assert_eq!(parents_of_head(&mut m).await, 1);
        assert_eq!(
            text(
                &mut m,
                "SELECT parent_hash FROM dolt_commit_ancestors
                  WHERE commit_hash = (SELECT commit_hash FROM dolt_log() LIMIT 1)"
            )
            .await,
            Some(main_before)
        );
        assert_eq!(
            texts(&mut m, "SELECT message FROM dolt_log()").await,
            [
                "Initialize data repository",
                "Merge branch 'draft' into main",
                "base",
                "edit on main"
            ],
            "the draft's own commits are not on main"
        );
    }

    #[tokio::test]
    async fn a_merge_commit_and_a_squash_both_revert() {
        for how in ["'draft'", "'--squash', 'draft'"] {
            let (s, _) = store_with_draft().await;
            both_edit_riker(&s, "note", "first officer").await;
            let mut m = s.rw().await;
            ok(&mut m, &format!("SELECT dolt_merge({how})")).await;
            let save = head(&mut m).await;
            ok(&mut m, &format!("SELECT dolt_revert('{save}')")).await;
            let row: (String, String) = sqlx::query_as("SELECT name, note FROM p WHERE id = 'r'")
                .fetch_one(&mut m)
                .await
                .unwrap();
            assert_eq!(row, ("Riker".into(), "first officer".into()), "{how}");
        }
    }

    #[tokio::test]
    async fn dash_d_refuses_unmerged_commits_and_capital_d_drops_them_with_the_working_set() {
        let (s, _) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        commit(&mut d, "rename").await;
        ok(&mut d, "UPDATE p SET note = 'dirty' WHERE id = 'r'").await;
        d.close().await.unwrap();

        let mut m = s.rw().await;
        err_contains(
            exec(&mut m, "SELECT dolt_branch('-d', 'draft')").await,
            "branch is not fully merged",
        );
        ok(&mut m, "SELECT dolt_branch('-D', 'draft')").await;
        assert_eq!(
            texts(&mut m, "SELECT name FROM dolt_branches").await,
            ["main"]
        );
        ok(&mut m, "SELECT dolt_branch('draft')").await;
        m.close().await.unwrap();
        let mut d = on_draft(&s).await;
        assert_eq!(
            int(&mut d, "SELECT COUNT(*) FROM dolt_status").await,
            0,
            "a branch made again under the old name starts clean"
        );
    }

    #[tokio::test]
    async fn dash_d_drops_a_branch_whose_only_change_is_uncommitted() {
        let (s, _) = store_with_draft().await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        d.close().await.unwrap();

        let mut m = s.rw().await;
        ok(&mut m, "SELECT dolt_branch('-d', 'draft')").await;
        assert_eq!(
            texts(&mut m, "SELECT name FROM dolt_branches").await,
            ["main"]
        );
    }
    /// The draft renames Riker and commits; `main` renames him too and
    /// changes his note, so the row conflicts on `name` alone.
    async fn a_conflict_beside_a_clean_change(s: &Store) {
        let mut d = on_draft(s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        commit(&mut d, "rename").await;
        d.close().await.unwrap();
        let mut m = s.rw().await;
        ok(
            &mut m,
            "UPDATE p SET name = 'Number One', note = 'first officer' WHERE id = 'r'",
        )
        .await;
        commit(&mut m, "edit on main").await;
        m.close().await.unwrap();
    }

    async fn riker(c: &mut SqliteConnection) -> (String, String) {
        sqlx::query_as("SELECT name, note FROM p WHERE id = 'r'")
            .fetch_one(c)
            .await
            .unwrap()
    }

    /// Guards a save that resolves with `dolt_conflicts_resolve`: it would
    /// drop what `main` changed in the conflicting row's other cells.
    #[tokio::test]
    async fn resolving_as_theirs_takes_the_whole_row_not_the_conflicting_cell() {
        let (s, _) = store_with_draft().await;
        a_conflict_beside_a_clean_change(&s).await;
        let mut m = s.rw().await;
        ok(&mut m, "BEGIN").await;
        err_contains(
            exec(&mut m, "SELECT dolt_merge('draft')").await,
            "Merge has 1 conflict(s)",
        );
        ok(&mut m, "SELECT dolt_conflicts_resolve('--theirs', 'p')").await;
        assert_eq!(riker(&mut m).await, ("Will Riker".into(), "x".into()));
    }

    #[tokio::test]
    async fn during_a_conflicted_merge_the_table_holds_ours_and_the_conflict_rows_every_side() {
        let (s, _) = store_with_draft().await;
        a_conflict_beside_a_clean_change(&s).await;
        let mut m = s.rw().await;
        ok(&mut m, "BEGIN").await;
        err_contains(
            exec(&mut m, "SELECT dolt_merge('draft')").await,
            "Merge has 1 conflict(s)",
        );
        assert_eq!(
            riker(&mut m).await,
            ("Number One".into(), "first officer".into())
        );
        let sides: (String, String, String, String, String, String) = sqlx::query_as(
            "SELECT base_name, base_note, our_name, our_note, their_name, their_note
               FROM dolt_conflicts_p",
        )
        .fetch_one(&mut m)
        .await
        .unwrap();
        assert_eq!(
            sides,
            (
                "Riker".into(),
                "x".into(),
                "Number One".into(),
                "first officer".into(),
                "Will Riker".into(),
                "x".into()
            )
        );
    }

    /// The save: write the draft's side of each cell the draft changed,
    /// then clear the conflict rows. Both merges, squash too.
    #[tokio::test]
    async fn writing_the_cells_then_deleting_the_conflict_rows_commits_both_changes() {
        for (how, parents) in [("'draft'", 2), ("'--squash', 'draft'", 1)] {
            let (s, _) = store_with_draft().await;
            a_conflict_beside_a_clean_change(&s).await;
            let mut m = s.rw().await;
            ok(&mut m, "BEGIN").await;
            err_contains(
                exec(&mut m, &format!("SELECT dolt_merge({how})")).await,
                "Merge has 1 conflict(s)",
            );
            ok(
                &mut m,
                "UPDATE p SET name = (SELECT their_name FROM dolt_conflicts_p
                                       WHERE our_id = 'r')
                  WHERE id = 'r'",
            )
            .await;
            ok(&mut m, "DELETE FROM dolt_conflicts_p").await;
            assert_eq!(int(&mut m, "SELECT COUNT(*) FROM dolt_conflicts").await, 0);
            commit(&mut m, "save").await;

            assert_eq!(
                riker(&mut m).await,
                ("Will Riker".into(), "first officer".into()),
                "{how}"
            );
            assert_eq!(parents_of_head(&mut m).await, parents, "{how}");
            assert_eq!(
                int(&mut m, "SELECT COUNT(*) FROM dolt_status").await,
                0,
                "{how}"
            );
        }
    }

    #[tokio::test]
    async fn a_draft_connection_reads_main_and_both_diffs_without_touching_its_rows() {
        let (s, cut) = store_with_draft().await;
        both_edit_riker(&s, "note", "first officer").await;
        let mut d = on_draft(&s).await;
        ok(&mut d, "UPDATE p SET note = 'dirty' WHERE id = 'w'").await;

        let at_main: Vec<(String, String)> =
            sqlx::query_as("SELECT id, note FROM dolt_at_p('main') ORDER BY id")
                .fetch_all(&mut d)
                .await
                .unwrap();
        assert_eq!(
            at_main,
            [
                ("r".to_string(), "first officer".to_string()),
                ("w".to_string(), "y".to_string())
            ]
        );
        assert_eq!(
            text(&mut d, "SELECT dolt_merge_base('draft', 'main')").await,
            Some(cut.clone())
        );
        let theirs: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT to_id, to_note FROM dolt_diff_p('{cut}', 'main')"
        )))
        .fetch_all(&mut d)
        .await
        .unwrap();
        assert_eq!(theirs, [("r".to_string(), "first officer".to_string())]);
        let mine: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT to_id, to_name FROM dolt_diff_p('{cut}', 'WORKING') ORDER BY to_id"
        )))
        .fetch_all(&mut d)
        .await
        .unwrap();
        assert_eq!(
            mine,
            [
                ("r".to_string(), "Will Riker".to_string()),
                ("w".to_string(), "Worf".to_string())
            ]
        );
        assert_eq!(
            text(&mut d, "SELECT note FROM p WHERE id = 'w'")
                .await
                .as_deref(),
            Some("dirty"),
            "still the draft's uncommitted row"
        );
        assert_eq!(
            text(&mut d, "SELECT active_branch()").await.as_deref(),
            Some("draft")
        );
    }

    /// The draft renames Riker and commits, leaving `main` where it was.
    async fn only_the_draft_moved(s: &Store) {
        let mut d = on_draft(s).await;
        ok(&mut d, "UPDATE p SET name = 'Will Riker' WHERE id = 'r'").await;
        commit(&mut d, "rename").await;
        d.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_squash_onto_a_branch_that_has_not_moved_stops_uncommitted() {
        let (s, cut) = store_with_draft().await;
        only_the_draft_moved(&s).await;
        let mut m = s.rw().await;
        ok(&mut m, "SELECT dolt_merge('--squash', 'draft')").await;
        assert_eq!(head(&mut m).await, cut, "nothing committed");
        assert_eq!(name_of(&mut m, "r").await.as_deref(), Some("Will Riker"));
        assert_eq!(
            texts(&mut m, "SELECT table_name FROM dolt_status").await,
            ["p"]
        );
    }

    #[tokio::test]
    async fn a_squash_with_no_commit_stops_uncommitted_whether_or_not_main_moved() {
        for main_moved in [false, true] {
            let (s, _) = store_with_draft().await;
            if main_moved {
                both_edit_riker(&s, "note", "first officer").await;
            } else {
                only_the_draft_moved(&s).await;
            }
            let mut m = s.rw().await;
            let before = head(&mut m).await;
            ok(
                &mut m,
                "SELECT dolt_merge('--squash', '--no-commit', 'draft')",
            )
            .await;
            assert_eq!(head(&mut m).await, before, "main moved: {main_moved}");
            assert_eq!(
                name_of(&mut m, "r").await.as_deref(),
                Some("Will Riker"),
                "main moved: {main_moved}"
            );
            commit(&mut m, "saved Riker").await;
            assert_eq!(parents_of_head(&mut m).await, 1, "main moved: {main_moved}");
        }
    }

    #[tokio::test]
    async fn a_branch_name_may_hold_a_slash() {
        let (s, _) = store_with_rows().await;
        let mut c = s.rw().await;
        ok(&mut c, "SELECT dolt_branch('draft/riker')").await;
        ok(&mut c, "SELECT dolt_connect_branch('draft/riker')").await;
        assert_eq!(
            text(&mut c, "SELECT active_branch()").await.as_deref(),
            Some("draft/riker")
        );
    }
}

// ── Query plans and indexes ─────────────────────────────────────────

mod plans {
    use super::*;

    #[tokio::test]
    async fn dolt_at_seeks_the_key_and_ignores_secondary_indexes() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "CREATE INDEX t_v ON t (v)").await;
        commit(&mut w, "index").await;
        let by_key = plan(&mut w, "SELECT * FROM dolt_at_t('HEAD') WHERE id = 'a'").await;
        let by_indexed = plan(&mut w, "SELECT * FROM dolt_at_t('HEAD') WHERE v = 1").await;
        assert_ne!(
            by_key, by_indexed,
            "dolt_at_ no longer seeks the key: {by_key}"
        );
        assert!(
            !by_indexed.contains("USING INDEX"),
            "dolt_at_ now uses t_v: {by_indexed}"
        );
        let ordered = plan(&mut w, "SELECT * FROM dolt_at_t('HEAD') ORDER BY v").await;
        assert!(
            ordered.contains("TEMP B-TREE"),
            "dolt_at_ now consumes ORDER BY: {ordered}"
        );
    }

    /// `every_filter_key_is_served_by_an_index` rests on this plan.
    #[tokio::test]
    async fn an_unindexed_filter_scans_and_sorts() {
        let s = Store::new();
        let mut w = s.rw().await;
        ok(
            &mut w,
            "CREATE TABLE g (uuid TEXT PRIMARY KEY, touched TEXT, org TEXT)",
        )
        .await;
        ok(&mut w, "CREATE INDEX g_touched ON g (touched, uuid)").await;
        let p = plan(
            &mut w,
            "SELECT uuid FROM g WHERE org = 'x' ORDER BY touched DESC, uuid DESC",
        )
        .await;
        assert!(p.contains("SCAN g") && p.contains("TEMP B-TREE"), "{p}");
        let unfiltered = plan(
            &mut w,
            "SELECT uuid FROM g ORDER BY touched DESC, uuid DESC",
        )
        .await;
        assert!(
            unfiltered.contains("g_touched") && !unfiltered.contains("TEMP B-TREE"),
            "{unfiltered}"
        );
    }

    #[tokio::test]
    async fn rowid_on_a_text_key_is_a_hash_not_an_order() {
        let (s, _) = store_with_rows().await;
        let mut r = s.ro().await;
        let a = int(&mut r, "SELECT rowid FROM t WHERE id = 'a'").await;
        let b = int(&mut r, "SELECT rowid FROM t WHERE id = 'b'").await;
        assert!(!(a == 1 && b == 2), "rowids count up again: {a}, {b}");
    }

    #[tokio::test]
    async fn dbstat_is_not_supported() {
        let (s, _) = store_with_rows().await;
        let mut r = s.ro().await;
        err_contains(exec(&mut r, "SELECT * FROM dbstat").await, "not supported");
    }
}

// ── Disk space and dolt_gc ──────────────────────────────────────────

mod gc {
    use super::*;

    async fn fill(w: &mut SqliteConnection) {
        ok(w, "CREATE TABLE big (id INTEGER PRIMARY KEY, body TEXT)").await;
        ok(
            w,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3000) \
             INSERT INTO big SELECT i, hex(randomblob(150)) FROM n",
        )
        .await;
    }

    #[tokio::test]
    async fn a_delete_reclaims_nothing_and_a_squash_does() {
        let s = Store::new();
        let mut w = s.rw().await;
        let init = head(&mut w).await;
        fill(&mut w).await;
        let full = commit(&mut w, "fill").await;
        ok(&mut w, "DELETE FROM big").await;
        commit(&mut w, "empty").await;
        ok(&mut w, "SELECT dolt_gc()").await;
        let after_delete = s.size();
        assert!(
            after_delete > 500_000,
            "a delete and gc reclaimed the rows: {after_delete} bytes"
        );
        assert_eq!(
            int(
                &mut w,
                &format!("SELECT COUNT(*) FROM dolt_at_big('{full}')")
            )
            .await,
            3000,
            "a commit's rows did not survive gc"
        );

        ok(&mut w, &format!("SELECT dolt_reset('--soft', '{init}')")).await;
        commit(&mut w, "squashed").await;
        ok(&mut w, "SELECT dolt_gc()").await;
        assert!(
            s.size() * 5 < after_delete,
            "a squash and gc kept {} bytes",
            s.size()
        );
    }

    #[tokio::test]
    async fn gc_may_run_before_the_commit() {
        let s = Store::new();
        let mut w = s.rw().await;
        fill(&mut w).await;
        ok(&mut w, "SELECT dolt_gc()").await;
        commit(&mut w, "after gc").await;
        assert_eq!(
            int(&mut w, "SELECT COUNT(*) FROM dolt_at_big('HEAD')").await,
            3000
        );
    }
}

// ── Full-text search (FTS5) ─────────────────────────────────────────

mod fts5 {
    use super::*;

    /// The layout the grid's terms index uses: a plain table holding each
    /// term with its document, and an FTS5 index over the term's value
    /// alone, linked by `term_id`. `@ . - _ + :` stay inside a token, so
    /// an id or an address is one token.
    async fn terms_store() -> (Store, SqliteConnection) {
        let s = Store::new();
        let mut c = s.rw().await;
        ok(
            &mut c,
            "CREATE TABLE terms (term_id INTEGER PRIMARY KEY, markdown_uuid TEXT, \
             uuid TEXT, kind TEXT, value TEXT)",
        )
        .await;
        ok(&mut c, "CREATE INDEX terms_by_md ON terms (markdown_uuid)").await;
        ok(
            &mut c,
            "CREATE VIRTUAL TABLE terms_fts USING fts5(value, content='', \
             contentless_delete=1, tokenize=\"unicode61 tokenchars '@.-_+:'\")",
        )
        .await;
        (s, c)
    }

    async fn add(c: &mut SqliteConnection, id: i64, md: &str, uuid: &str, kind: &str, v: &str) {
        ok(
            c,
            &format!(
                "INSERT INTO terms VALUES ({id}, '{md}', '{uuid}', '{kind}', '{v}'); \
                 INSERT INTO terms_fts (rowid, value) VALUES ({id}, '{v}')"
            ),
        )
        .await;
    }

    async fn matching(c: &mut SqliteConnection, q: &str) -> Vec<String> {
        texts(
            c,
            &format!(
                "SELECT t.uuid || ':' || t.kind FROM terms_fts f \
                 JOIN terms t ON t.term_id = f.rowid WHERE terms_fts MATCH '{q}'"
            ),
        )
        .await
    }

    async fn two_documents() -> (Store, SqliteConnection) {
        let (s, mut c) = terms_store().await;
        add(
            &mut c,
            1,
            "d1",
            "r1",
            "id",
            "00000000-0000-8b8a-896d-63addc7b31ad",
        )
        .await;
        add(&mut c, 2, "d1", "r1", "from", "email:ann@example.com").await;
        add(&mut c, 3, "d1", "r1", "title", "Quarterly budget review").await;
        add(&mut c, 4, "d2", "r2", "to", "email:ann@example.com").await;
        add(&mut c, 5, "d2", "r2", "to", "email:ann@example.com.au").await;
        (s, c)
    }

    #[tokio::test]
    async fn an_id_or_an_address_is_one_token_and_matches_only_itself() {
        let (_s, mut c) = two_documents().await;
        assert_eq!(
            matching(&mut c, "\"email:ann@example.com\"").await,
            ["r1:from", "r2:to"]
        );
        assert_eq!(
            matching(&mut c, "\"00000000-0000-8b8a-896d-63addc7b31ad\"").await,
            ["r1:id"]
        );
        assert_eq!(matching(&mut c, "\"00000000\"").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_prefix_query_matches_the_start_of_a_word() {
        let (_s, mut c) = two_documents().await;
        assert_eq!(matching(&mut c, "budg*").await, ["r1:title"]);
        assert_eq!(matching(&mut c, "udget*").await, Vec::<String>::new());
    }

    /// The join back to the plain table seeks its key for every hit, so a
    /// `kind` filter costs nothing beyond the match.
    #[tokio::test]
    async fn the_join_back_to_the_terms_seeks_their_key() {
        let (_s, mut c) = two_documents().await;
        let p = plan(
            &mut c,
            "SELECT t.uuid FROM terms_fts f JOIN terms t ON t.term_id = f.rowid \
             WHERE terms_fts MATCH 'x' AND t.kind = 'to'",
        )
        .await;
        assert!(p.contains("SEARCH t USING INTEGER PRIMARY KEY"), "{p}");
    }

    /// Replacing a document's terms: its `term_id`s come from the plain
    /// table's index, and the FTS5 index forgets each one by rowid.
    #[tokio::test]
    async fn a_contentless_delete_index_forgets_a_documents_terms_by_rowid() {
        let (_s, mut c) = two_documents().await;
        ok(
            &mut c,
            "DELETE FROM terms_fts WHERE rowid IN \
             (SELECT term_id FROM terms WHERE markdown_uuid = 'd1'); \
             DELETE FROM terms WHERE markdown_uuid = 'd1'",
        )
        .await;
        assert_eq!(
            matching(&mut c, "\"email:ann@example.com\"").await,
            ["r2:to"]
        );
        assert_eq!(matching(&mut c, "budget").await, Vec::<String>::new());
    }

    /// `term_id` is a plain counter: an `INTEGER PRIMARY KEY` is the rowid
    /// itself, and one left `NULL` is the next one up, unlike a text key's
    /// rowid (`rowid_on_a_text_key_is_a_hash_not_an_order`).
    #[tokio::test]
    async fn an_integer_primary_key_is_the_rowid_and_counts_up() {
        let (_s, mut c) = two_documents().await;
        ok(
            &mut c,
            "INSERT INTO terms (term_id, markdown_uuid) VALUES (NULL, 'd3')",
        )
        .await;
        assert_eq!(
            int(
                &mut c,
                "SELECT term_id FROM terms WHERE markdown_uuid = 'd3'"
            )
            .await,
            6
        );
        assert_eq!(
            int(&mut c, "SELECT COUNT(*) FROM terms WHERE rowid != term_id").await,
            0
        );
    }

    /// The grid's readers open `<file>@<hash>`, and the applet holds a read
    /// transaction; each matches the terms of the commit it reads.
    #[tokio::test]
    async fn a_reader_of_one_commit_matches_that_commits_terms() {
        let (s, mut c) = two_documents().await;
        let first = commit(&mut c, "first").await;
        add(&mut c, 6, "d3", "r3", "label", "work").await;
        commit(&mut c, "second").await;

        let mut d = connect(&s.at(&first), true, Duration::from_secs(5)).await;
        assert_eq!(matching(&mut d, "work").await, Vec::<String>::new());
        assert_eq!(matching(&mut d, "budget").await, ["r1:title"]);

        let mut r = s.ro().await;
        ok(&mut r, "BEGIN").await;
        assert_eq!(matching(&mut r, "work").await, ["r3:label"]);
        ok(&mut r, "COMMIT").await;
    }

    /// An FTS5 table is matched by its own name, not by an alias.
    #[tokio::test]
    async fn an_fts5_table_is_matched_by_name_not_by_alias() {
        let (_s, mut c) = two_documents().await;
        assert_eq!(
            int(
                &mut c,
                "SELECT COUNT(*) FROM terms_fts AS f WHERE terms_fts MATCH 'budget'"
            )
            .await,
            1
        );
        err_contains(
            exec(
                &mut c,
                "SELECT COUNT(*) FROM terms_fts AS f WHERE f MATCH 'budget'",
            )
            .await,
            "no such column: f",
        );
    }

    /// The terms can live in a plain SQLite file beside the store, which
    /// keeps no history of them: attached to a reader of one commit, its
    /// FTS5 index matches and the joins on both sides seek their keys.
    #[tokio::test]
    async fn a_plain_sqlite_terms_file_attached_to_a_commit_matches_and_joins_by_key() {
        let s = Store::new();
        let plain = s.path.with_file_name("terms.sqlite");
        let mut t = connect(
            &format!("file:{}?doltlite_engine=sqlite", plain.display()),
            false,
            Duration::from_secs(5),
        )
        .await;
        ok(
            &mut t,
            "CREATE TABLE terms (term_id INTEGER PRIMARY KEY, uuid TEXT, kind TEXT, value TEXT); \
             CREATE VIRTUAL TABLE terms_fts USING fts5(value, content='', contentless_delete=1, \
             tokenize=\"unicode61 tokenchars '@.-_+:'\"); \
             INSERT INTO terms VALUES (1, 'r1', 'to', 'email:ann@example.com'), \
             (2, 'r2', 'from', 'email:ann@example.com'); \
             INSERT INTO terms_fts (rowid, value) SELECT term_id, value FROM terms",
        )
        .await;
        t.close().await.expect("close");

        let mut w = s.rw().await;
        ok(
            &mut w,
            "CREATE TABLE grid_rows (uuid TEXT PRIMARY KEY, touched TEXT); \
             INSERT INTO grid_rows VALUES ('r1', '2026-01-02'), ('r2', '2026-01-01')",
        )
        .await;
        let first = commit(&mut w, "rows").await;
        ok(&mut w, "DELETE FROM grid_rows WHERE uuid = 'r1'").await;
        commit(&mut w, "r1 gone").await;

        let mut d = connect(&s.at(&first), true, Duration::from_secs(5)).await;
        ok(
            &mut d,
            &format!(
                "ATTACH 'file:{}?doltlite_engine=sqlite&mode=ro' AS t",
                plain.display()
            ),
        )
        .await;
        let q = "SELECT g.uuid FROM t.terms_fts JOIN t.terms x ON x.term_id = terms_fts.rowid \
                 JOIN grid_rows g ON g.uuid = x.uuid \
                 WHERE terms_fts MATCH '\"email:ann@example.com\"'";
        assert_eq!(texts(&mut d, q).await, ["r1", "r2"]);
        let p = plan(&mut d, q).await;
        assert!(p.contains("SEARCH x USING INTEGER PRIMARY KEY"), "{p}");
        assert!(p.contains("SEARCH g USING"), "{p}");
    }
}

// ── Plain SQLite files and SQLite compatibility ─────────────────────

mod compat {
    use super::*;

    #[tokio::test]
    async fn the_file_is_chunk_store_format_12() {
        let (s, _) = store_with_rows().await;
        let bytes = std::fs::read(&s.path).unwrap();
        assert_eq!(&bytes[..4], b"CTLD", "not a chunk store");
        assert_eq!(
            bytes[4], 12,
            "the chunk-store format moved: every existing store is refused"
        );
    }

    #[tokio::test]
    async fn doltlite_engine_sqlite_writes_a_plain_sqlite_file() {
        let (s, _) = store_with_rows().await;
        let plain = s.path.with_file_name("plain.sqlite");
        let mut w = s.rw().await;
        ok(
            &mut w,
            &format!(
                "ATTACH 'file:{}?doltlite_engine=sqlite' AS out",
                plain.display()
            ),
        )
        .await;
        ok(&mut w, "CREATE TABLE out.t AS SELECT * FROM main.t").await;
        ok(&mut w, "DETACH out").await;
        let bytes = std::fs::read(&plain).unwrap();
        assert_eq!(&bytes[..16], b"SQLite format 3\0");
    }

    #[tokio::test]
    async fn journal_mode_is_inert() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        assert_eq!(
            text(&mut w, "PRAGMA journal_mode = DELETE")
                .await
                .as_deref(),
            Some("wal")
        );
        let wal = s.path.with_file_name("store.doltlite_db-wal");
        assert!(!wal.exists(), "doltlite made a -wal sidecar");
    }

    /// Doltlite cannot write a WAL, but it reads one: a stock-SQLite file
    /// in WAL mode, as qmd's index is between checkpoints, reads with the
    /// rows its `-wal` holds. Without the `-wal` the same file holds only
    /// what was checkpointed. `scripts/make_doltlite_wal_fixture.py` wrote the
    /// pair.
    #[tokio::test]
    async fn a_plain_sqlite_file_in_wal_mode_is_read_with_its_wal() {
        let fixture = |name: &str| {
            let dir =
                std::env::var("WAL_FIXTURE_DIR").expect("WAL_FIXTURE_DIR is set by the BUILD rule");
            PathBuf::from(dir)
                .join(name)
                .canonicalize()
                .expect("the fixture is in runfiles")
        };
        let count = |dir: &Path| {
            let db = dir.join("wal.sqlite");
            async move {
                let mut c = connect(&db.display().to_string(), true, Duration::from_secs(5)).await;
                int(&mut c, "SELECT COUNT(*) FROM crew").await
            }
        };
        let both = tempfile::tempdir().unwrap();
        let main_only = tempfile::tempdir().unwrap();
        for name in ["wal.sqlite", "wal.sqlite-wal"] {
            std::fs::copy(fixture(name), both.path().join(name)).unwrap();
        }
        std::fs::copy(fixture("wal.sqlite"), main_only.path().join("wal.sqlite")).unwrap();
        assert_eq!(
            &std::fs::read(both.path().join("wal.sqlite")).unwrap()[18..20],
            &[2, 2]
        );

        assert_eq!(count(both.path()).await, 3);
        assert_eq!(count(main_only.path()).await, 1);
    }

    /// A plain SQLite file opened through doltlite answers `wal` too, but
    /// keeps a rollback journal: its header never says WAL (bytes 18 and
    /// 19 stay 1), so a reader waits while its writer commits.
    #[tokio::test]
    async fn a_plain_sqlite_file_keeps_a_rollback_journal_whatever_it_answers() {
        let s = Store::new();
        let plain = s.path.with_file_name("plain.sqlite");
        let mut c = connect(
            &format!("file:{}?doltlite_engine=sqlite", plain.display()),
            false,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            text(&mut c, "PRAGMA journal_mode = WAL").await.as_deref(),
            Some("wal")
        );
        ok(&mut c, "CREATE TABLE a (x)").await;
        ok(&mut c, "INSERT INTO a VALUES (1)").await;
        c.close().await.expect("close");
        let bytes = std::fs::read(&plain).unwrap();
        assert_eq!(&bytes[..16], b"SQLite format 3\0");
        assert_eq!(&bytes[18..20], &[1, 1], "the header now says WAL");
    }

    #[tokio::test]
    async fn a_text_primary_key_is_not_null() {
        let (s, _) = store_with_rows().await;
        let mut r = s.ro().await;
        assert_eq!(
            int(
                &mut r,
                "SELECT \"notnull\" FROM pragma_table_info('t') WHERE name = 'id'"
            )
            .await,
            1
        );
        assert_eq!(
            int(
                &mut r,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_autoindex_t_1'"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn fts5_and_rtree_are_compiled_in() {
        let s = Store::new();
        let mut w = s.rw().await;
        ok(&mut w, "CREATE VIRTUAL TABLE f USING fts5(body)").await;
        ok(&mut w, "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1)").await;
    }

    #[tokio::test]
    async fn a_new_file_starts_with_one_commit() {
        let s = Store::new();
        let mut w = s.rw().await;
        assert_eq!(
            texts(&mut w, "SELECT message FROM dolt_log()").await,
            vec!["Initialize data repository"]
        );
    }

    #[tokio::test]
    async fn a_hard_reset_keeps_an_untracked_table_and_clean_removes_it() {
        let (s, _) = store_with_rows().await;
        let mut w = s.rw().await;
        ok(&mut w, "UPDATE t SET v = 99").await;
        ok(&mut w, "CREATE TABLE u (id TEXT PRIMARY KEY)").await;
        ok(&mut w, "SELECT dolt_reset('--hard')").await;
        assert_eq!(int(&mut w, "SELECT SUM(v) FROM t").await, 3);
        assert_eq!(
            int(
                &mut w,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'u'"
            )
            .await,
            1
        );
        ok(&mut w, "SELECT dolt_clean()").await;
        assert_eq!(
            int(
                &mut w,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'u'"
            )
            .await,
            0
        );
    }

    /// Why `doltlite_raw` skips the reset when the store has only its
    /// initialization commit.
    #[tokio::test]
    async fn a_hard_reset_at_the_first_commit_breaks_the_clean_after_it() {
        let s = Store::new();
        let mut w = s.rw().await;
        ok(
            &mut w,
            "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
        )
        .await;
        ok(&mut w, "INSERT INTO a (v) VALUES ('x')").await;
        ok(&mut w, "SELECT dolt_reset('--hard')").await;
        err_contains(exec(&mut w, "SELECT dolt_clean()").await, "sqlite_sequence");
    }
}
