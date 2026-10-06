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
