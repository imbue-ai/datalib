//! A record's unsaved edits, kept on a branch of its own store, and the
//! save that merges them back with the draft winning every cell it
//! changed. The store's one writer does all of it, on its own pool: a
//! process that moves a ref is a writer (AGENTS.md § "Doltlite"). The
//! doltlite behaviour underneath is pinned in docs/dev/doltlite.md
//! § "Merging a branch"; why a store wants drafts is
//! docs/dev/plans/contact_editing.md.

use anyhow::{bail, Context, Result};
use sqlx::pool::PoolConnection;
use sqlx::{Sqlite, SqliteConnection, SqlitePool};

use crate::doltlite_raw::{self, WRITER_BRANCH};

/// The branch the draft of `key` lives on.
pub fn branch_of(key: &str) -> String {
    format!("draft/{key}")
}

pub async fn exists(pool: &SqlitePool, branch: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_branches WHERE name = ?")
        .bind(branch)
        .fetch_one(pool)
        .await
        .context("list the store's branches")?;
    Ok(n > 0)
}

/// Every draft branch in the store.
pub async fn all(pool: &SqlitePool) -> Result<Vec<String>> {
    sqlx::query_scalar("SELECT name FROM dolt_branches WHERE name LIKE 'draft/%' ORDER BY name")
        .fetch_all(pool)
        .await
        .context("list the store's drafts")
}

/// Cut `branch` from the writer's last seal, unless it is there already.
/// Returns whether it was cut.
pub async fn cut(pool: &SqlitePool, branch: &str) -> Result<bool> {
    if exists(pool, branch).await? {
        return Ok(false);
    }
    sqlx::query("SELECT dolt_branch(?)")
        .bind(branch)
        .execute(pool)
        .await
        .with_context(|| format!("cut the draft {branch}"))?;
    Ok(true)
}

/// The commit `branch` was cut from, or last took the writer's work at.
pub async fn base(pool: &SqlitePool, branch: &str) -> Result<String> {
    sqlx::query_scalar("SELECT dolt_merge_base(?, ?)")
        .bind(branch)
        .bind(WRITER_BRANCH)
        .fetch_one(pool)
        .await
        .with_context(|| format!("find where the draft {branch} was cut"))
}

/// The writer's last seal: what every reader sees.
pub async fn published(pool: &SqlitePool) -> Result<String> {
    sqlx::query_scalar("SELECT dolt_hashof(?)")
        .bind(WRITER_BRANCH)
        .fetch_one(pool)
        .await
        .context("read the writer's head")
}

/// The writer's connection, moved onto a draft branch. Reads see the
/// draft's rows, uncommitted ones included, and writes stay on the
/// draft uncommitted until [`save`]. [`OnDraft::leave`] puts the
/// connection back on the writer's branch; a guard dropped without
/// leaving closes the connection instead, so the pool can never hand
/// out a connection still on a draft.
pub struct OnDraft {
    conn: PoolConnection<Sqlite>,
    left: bool,
}

impl OnDraft {
    pub async fn enter(pool: &SqlitePool, branch: &str) -> Result<OnDraft> {
        let conn = pool
            .acquire()
            .await
            .context("take the store's connection")?;
        let mut on = OnDraft { conn, left: false };
        sqlx::query("SELECT dolt_connect_branch(?)")
            .bind(branch)
            .execute(&mut *on.conn)
            .await
            .with_context(|| format!("move onto the draft {branch}"))?;
        Ok(on)
    }

    pub fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.conn
    }

    pub async fn leave(mut self) -> Result<()> {
        sqlx::query("SELECT dolt_connect_branch(?)")
            .bind(WRITER_BRANCH)
            .execute(&mut *self.conn)
            .await
            .context("move back onto the writer's branch")?;
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&mut *self.conn)
            .await
            .context("read the branch back")?;
        if active != WRITER_BRANCH {
            bail!("left a draft for {active:?}, not {WRITER_BRANCH:?}");
        }
        self.left = true;
        Ok(())
    }
}

impl Drop for OnDraft {
    fn drop(&mut self) {
        if !self.left {
            tracing::warn!("a draft's connection was dropped on the draft; closing it");
            self.conn.close_on_drop();
        }
    }
}

/// Merge `branch` into the writer's branch as one commit with `message`,
/// publish it, and delete the branch. Where the draft and the writer
/// both changed a cell, the draft's value is kept, and every other
/// change on either side survives. `None` when the draft changed
/// nothing; the branch goes either way.
pub async fn save(pool: &SqlitePool, branch: &str, message: &str) -> Result<Option<String>> {
    let mut on = OnDraft::enter(pool, branch).await?;
    let committed = commit_quietly(on.conn(), "draft").await;
    on.leave().await?;
    committed?;

    let mut conn = pool
        .acquire()
        .await
        .context("take the store's connection")?;
    let saved =
        merge_draft_wins(&mut conn, &doltlite_raw::store_label(pool), branch, message).await;
    if saved.is_err() {
        // Best effort: a failure before the commit leaves the merge's
        // transaction open, and the next statement must not land in it.
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
    }
    drop(conn);
    let saved = saved?;
    discard(pool, branch).await?;
    Ok(saved)
}

/// Throw the draft away, uncommitted rows and all.
pub async fn discard(pool: &SqlitePool, branch: &str) -> Result<()> {
    if !exists(pool, branch).await? {
        return Ok(());
    }
    sqlx::query("SELECT dolt_branch('-D', ?)")
        .bind(branch)
        .execute(pool)
        .await
        .with_context(|| format!("delete the draft {branch}"))?;
    Ok(())
}

async fn commit_quietly(conn: &mut SqliteConnection, message: &str) -> Result<()> {
    match sqlx::query("SELECT dolt_commit('-Am', ?)")
        .bind(message)
        .execute(&mut *conn)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("nothing to commit") => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("commit the draft")),
    }
}

async fn merge_draft_wins(
    conn: &mut SqliteConnection,
    store: &str,
    branch: &str,
    message: &str,
) -> Result<Option<String>> {
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .context("begin the save")?;
    // `--no-commit` so every case ends uncommitted — a fast-forward, a
    // clean merge, a conflicted one — and the one commit below carries
    // our message.
    match sqlx::query("SELECT dolt_merge('--squash', '--no-commit', ?)")
        .bind(branch)
        .execute(&mut *conn)
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("conflict") => draft_wins(conn).await?,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("merge the draft {branch}"))),
    }
    let saved = doltlite_raw::commit_on(conn, store, message, None).await?;
    if saved.is_none() {
        // A draft equal to the writer's state commits nothing, and may or
        // may not have left the transaction open.
        match sqlx::query("COMMIT").execute(&mut *conn).await {
            Ok(_) => {}
            Err(e) if e.to_string().contains("no transaction is active") => {}
            Err(e) => return Err(anyhow::Error::new(e).context("end an empty save")),
        }
    }
    Ok(saved)
}

/// Settle every conflict of the merge in progress cell by cell, the
/// draft's value winning wherever the draft changed it. Not
/// `dolt_conflicts_resolve('--theirs')`, which takes the draft's whole
/// row and loses the writer's change to its other cells.
async fn draft_wins(conn: &mut SqliteConnection) -> Result<()> {
    let tables: Vec<String> = sqlx::query_scalar("SELECT \"table\" FROM dolt_conflicts")
        .fetch_all(&mut *conn)
        .await
        .context("list the tables in conflict")?;
    for table in tables {
        let columns: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, pk FROM pragma_table_info(?) ORDER BY cid")
                .bind(&table)
                .fetch_all(&mut *conn)
                .await
                .with_context(|| format!("read the columns of {table}"))?;
        let columns: Vec<(String, bool)> = columns.into_iter().map(|(c, pk)| (c, pk > 0)).collect();
        for stmt in draft_wins_sql(&table, &columns) {
            // Audited: every name is quoted, and every one came from the
            // store's own catalog, not from a row.
            sqlx::query(sqlx::AssertSqlSafe(stmt.clone()))
                .execute(&mut *conn)
                .await
                .with_context(|| format!("settle a conflict in {table}: {stmt}"))?;
        }
    }
    Ok(())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The statements that settle `table`'s conflicts, the draft winning:
/// a row the draft deleted goes, a row the writer deleted and the draft
/// changed comes back as the draft has it, and in a row both kept each
/// cell the draft changed takes the draft's value. `columns` is every
/// column, with whether it is part of the key.
fn draft_wins_sql(table: &str, columns: &[(String, bool)]) -> Vec<String> {
    let t = quote(table);
    let conf = quote(&format!("dolt_conflicts_{table}"));
    let side = |prefix: &str, col: &str| format!("c.{}", quote(&format!("{prefix}{col}")));
    let keys: Vec<&str> = columns
        .iter()
        .filter(|(_, pk)| *pk)
        .map(|(c, _)| c.as_str())
        .collect();
    let same_row = keys
        .iter()
        .map(|k| format!("{} = {t}.{}", side("our_", k), quote(k)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let both_kept = "c.our_diff_type <> 'removed' AND c.their_diff_type <> 'removed'";
    let mut out = vec![
        format!(
            "DELETE FROM {t} WHERE EXISTS (SELECT 1 FROM {conf} c \
              WHERE c.their_diff_type = 'removed' AND c.our_diff_type <> 'removed' AND {same_row})"
        ),
        format!(
            "INSERT INTO {t} ({cols}) SELECT {theirs} FROM {conf} c \
              WHERE c.our_diff_type = 'removed' AND c.their_diff_type <> 'removed'",
            cols = columns
                .iter()
                .map(|(c, _)| quote(c))
                .collect::<Vec<_>>()
                .join(", "),
            theirs = columns
                .iter()
                .map(|(c, _)| side("their_", c))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    ];
    for (col, _) in columns.iter().filter(|(_, pk)| !*pk) {
        let (theirs, base) = (side("their_", col), side("base_", col));
        out.push(format!(
            "UPDATE {t} SET {c} = (SELECT {theirs} FROM {conf} c WHERE {same_row}) \
              WHERE EXISTS (SELECT 1 FROM {conf} c \
                WHERE {same_row} AND {both_kept} AND {theirs} IS NOT {base})",
            c = quote(col),
        ));
    }
    out.push(format!("DELETE FROM {conf}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDL: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS people (id TEXT PRIMARY KEY, name TEXT, note TEXT)",
        "CREATE TABLE IF NOT EXISTS members (
            group_id TEXT NOT NULL,
            member_id TEXT NOT NULL,
            photo BLOB,
            PRIMARY KEY (group_id, member_id)
        )",
    ];

    struct Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        pool: SqlitePool,
    }

    /// A store holding Riker and Worf, sealed.
    async fn store() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.doltlite_db");
        let pool = doltlite_raw::open(&path, DDL).await.unwrap();
        for sql in [
            "INSERT INTO people VALUES ('r', 'Riker', 'x'), ('w', 'Worf', 'y')",
            "INSERT INTO members VALUES ('away', 'r', x'00'), ('away', 'w', x'01')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        doltlite_raw::commit_run(&pool, "seed").await.unwrap();
        Fixture {
            _dir: dir,
            path,
            pool,
        }
    }

    async fn on_draft(pool: &SqlitePool, sql: &[&str]) {
        let mut on = OnDraft::enter(pool, "draft/r").await.unwrap();
        for s in sql {
            sqlx::query(sqlx::AssertSqlSafe(s.to_string()))
                .execute(on.conn())
                .await
                .unwrap();
        }
        on.leave().await.unwrap();
    }

    async fn sealed(pool: &SqlitePool, sql: &str) {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(pool)
            .await
            .unwrap();
        doltlite_raw::commit_run(pool, "elsewhere").await.unwrap();
    }

    async fn people(pool: &SqlitePool) -> Vec<(String, Option<String>, Option<String>)> {
        sqlx::query_as("SELECT id, name, note FROM people ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    fn row(id: &str, name: &str, note: &str) -> (String, Option<String>, Option<String>) {
        (id.into(), Some(name.into()), Some(note.into()))
    }

    async fn head(pool: &SqlitePool) -> (String, String, i64) {
        sqlx::query_as(
            "SELECT l.commit_hash, l.message,
                    (SELECT count(*) FROM dolt_commit_ancestors a WHERE a.commit_hash = l.commit_hash)
               FROM dolt_log() l LIMIT 1",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn main_head(pool: &SqlitePool) -> String {
        sqlx::query_scalar("SELECT dolt_hashof('main')")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_autosave_outlives_the_writer_and_nobody_else_sees_it() {
        let f = store().await;
        assert!(cut(&f.pool, "draft/r").await.unwrap());
        assert!(!cut(&f.pool, "draft/r").await.unwrap(), "cut once");
        on_draft(
            &f.pool,
            &["UPDATE people SET name = 'Will Riker' WHERE id = 'r'"],
        )
        .await;
        assert_eq!(people(&f.pool).await[0], row("r", "Riker", "x"));
        f.pool.close().await;

        // A writer's open resets its own branch's working set.
        let pool = doltlite_raw::open(&f.path, DDL).await.unwrap();
        let mut on = OnDraft::enter(&pool, "draft/r").await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM people WHERE id = 'r'")
            .fetch_one(on.conn())
            .await
            .unwrap();
        on.leave().await.unwrap();
        assert_eq!(name, "Will Riker");
        assert_eq!(all(&pool).await.unwrap(), ["draft/r"]);
    }

    #[tokio::test]
    async fn a_save_with_nothing_else_moved_is_one_published_commit() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &["UPDATE people SET name = 'Will Riker' WHERE id = 'r'"],
        )
        .await;
        let saved = save(&f.pool, "draft/r", "saved Riker").await.unwrap();

        let (hash, message, parents) = head(&f.pool).await;
        assert_eq!(saved, Some(hash.clone()));
        assert_eq!((message.as_str(), parents), ("saved Riker", 1));
        assert_eq!(main_head(&f.pool).await, hash, "published");
        assert_eq!(people(&f.pool).await[0], row("r", "Will Riker", "x"));
        assert!(!exists(&f.pool, "draft/r").await.unwrap());
    }

    /// Guards the save against `dolt_conflicts_resolve('--theirs')`,
    /// which would take the draft's whole row and lose the note.
    #[tokio::test]
    async fn the_draft_wins_its_cells_and_a_change_made_meanwhile_survives() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &["UPDATE people SET name = 'Will Riker' WHERE id = 'r'"],
        )
        .await;
        sealed(
            &f.pool,
            "UPDATE people SET name = 'Number One', note = 'first officer' WHERE id = 'r'",
        )
        .await;
        save(&f.pool, "draft/r", "saved Riker").await.unwrap();

        assert_eq!(
            people(&f.pool).await,
            [
                row("r", "Will Riker", "first officer"),
                row("w", "Worf", "y")
            ]
        );
        let (hash, message, parents) = head(&f.pool).await;
        assert_eq!((message.as_str(), parents), ("saved Riker", 1));
        assert_eq!(main_head(&f.pool).await, hash);
        let status: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_status")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(status, 0);
    }

    #[tokio::test]
    async fn edits_to_other_rows_merge_into_one_commit_with_our_message() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &["UPDATE people SET name = 'Will Riker' WHERE id = 'r'"],
        )
        .await;
        sealed(
            &f.pool,
            "UPDATE people SET note = 'security' WHERE id = 'w'",
        )
        .await;
        save(&f.pool, "draft/r", "saved Riker").await.unwrap();

        assert_eq!(
            people(&f.pool).await,
            [row("r", "Will Riker", "x"), row("w", "Worf", "security")]
        );
        let (_, message, parents) = head(&f.pool).await;
        assert_eq!((message.as_str(), parents), ("saved Riker", 1));
    }

    #[tokio::test]
    async fn a_row_one_side_deleted_and_the_other_changed_goes_the_drafts_way() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &[
                "DELETE FROM people WHERE id = 'r'",
                "UPDATE people SET name = 'Lt Worf' WHERE id = 'w'",
            ],
        )
        .await;
        sealed(&f.pool, "UPDATE people SET note = 'kept?' WHERE id = 'r'").await;
        sealed(&f.pool, "DELETE FROM people WHERE id = 'w'").await;
        save(&f.pool, "draft/r", "saved").await.unwrap();

        assert_eq!(people(&f.pool).await, [row("w", "Lt Worf", "y")]);
    }

    #[tokio::test]
    async fn a_composite_key_and_a_blob_merge_cell_by_cell() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &["UPDATE members SET photo = x'aa' WHERE group_id = 'away' AND member_id = 'r'"],
        )
        .await;
        sealed(
            &f.pool,
            "UPDATE members SET photo = x'bb' WHERE group_id = 'away'",
        )
        .await;
        save(&f.pool, "draft/r", "saved").await.unwrap();

        let photos: Vec<(String, Vec<u8>)> =
            sqlx::query_as("SELECT member_id, photo FROM members ORDER BY member_id")
                .fetch_all(&f.pool)
                .await
                .unwrap();
        assert_eq!(
            photos,
            [("r".to_string(), vec![0xaa]), ("w".to_string(), vec![0xbb])]
        );
    }

    #[tokio::test]
    async fn a_draft_that_changed_nothing_saves_nothing_and_goes() {
        let f = store().await;
        let before = head(&f.pool).await;
        cut(&f.pool, "draft/r").await.unwrap();
        assert_eq!(save(&f.pool, "draft/r", "saved").await.unwrap(), None);
        assert_eq!(head(&f.pool).await, before);
        assert!(!exists(&f.pool, "draft/r").await.unwrap());
        sealed(&f.pool, "UPDATE people SET note = 'z' WHERE id = 'w'").await;
    }

    #[tokio::test]
    async fn base_and_published_say_where_the_draft_and_the_writer_are() {
        let f = store().await;
        let seed = published(&f.pool).await.unwrap();
        cut(&f.pool, "draft/r").await.unwrap();
        sealed(&f.pool, "UPDATE people SET note = 'z' WHERE id = 'w'").await;
        assert_eq!(base(&f.pool, "draft/r").await.unwrap(), seed);
        assert_ne!(published(&f.pool).await.unwrap(), seed);
    }

    #[tokio::test]
    async fn discard_drops_the_draft_and_its_rows() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        on_draft(
            &f.pool,
            &["UPDATE people SET name = 'Will Riker' WHERE id = 'r'"],
        )
        .await;
        discard(&f.pool, "draft/r").await.unwrap();
        assert!(!exists(&f.pool, "draft/r").await.unwrap());
        cut(&f.pool, "draft/r").await.unwrap();
        let mut on = OnDraft::enter(&f.pool, "draft/r").await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM people WHERE id = 'r'")
            .fetch_one(on.conn())
            .await
            .unwrap();
        on.leave().await.unwrap();
        assert_eq!(name, "Riker");
    }

    /// Guards the pool against handing out a connection still on a
    /// draft, where the next seal would commit to the draft.
    #[tokio::test]
    async fn a_guard_dropped_on_the_draft_leaves_the_writer_on_its_branch() {
        let f = store().await;
        cut(&f.pool, "draft/r").await.unwrap();
        drop(OnDraft::enter(&f.pool, "draft/r").await.unwrap());
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(active, WRITER_BRANCH);
    }

    #[test]
    fn every_name_in_the_settling_sql_is_quoted() {
        let sql = draft_wins_sql("p", &[("id".into(), true), ("a\"b".into(), false)]);
        assert!(sql.iter().any(|s| s.contains("\"a\"\"b\"")), "{sql:?}");
        assert!(sql.iter().all(|s| !s.contains(" a\"b")), "{sql:?}");
    }
}
