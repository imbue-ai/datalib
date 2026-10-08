//! Deleting rows because upstream stopped serving them.
//!
//! A downloader that only ever upserts cannot tell "deleted" from "not
//! mentioned this time", so every prune here rests on the caller having
//! *re-enumerated* something completely: a PR's whole comment list, a
//! channel's history over a bounded time window, an account's whole
//! conversation index. What belongs in this file is the part that is the
//! same every time — the SQL, and the judgement about when a proposed
//! deletion is too large to believe.
//!
//! What does not belong here is the decision that an enumeration *was*
//! complete. That is provider knowledge and stays in the provider.

use std::collections::HashSet;

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Delete the rows of `table` inside `scope` whose id is not in `keep`,
/// their bookkeeping sidecars and their fetch problems. Returns the ids
/// that went.
///
/// `scope` is the bound the caller re-enumerated, as `(column, value)`
/// equality pairs — `[("repo_full_name", repo), ("pr_number", num)]` for one
/// PR's comments. An empty `scope` means the whole table, which is only
/// right when the caller enumerated the whole upstream collection.
///
/// Table and column names are interpolated; callers pass trusted
/// identifiers. Values are bound.
pub async fn prune_scope(
    pool: &SqlitePool,
    table: &str,
    scope: &[(&str, &str)],
    keep: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut tx = pool.begin().await.context("begin prune tx")?;
    let gone = prune_scope_in_tx(&mut tx, table, scope, keep).await?;
    tx.commit().await.context("commit prune tx")?;
    Ok(gone)
}

/// [`prune_scope`] inside the caller's transaction, so the rows that
/// replace a collection and the prune of what it lost land together.
pub async fn prune_scope_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    scope: &[(&str, &str)],
    keep: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut where_sql = String::new();
    for (i, (col, _)) in scope.iter().enumerate() {
        where_sql.push_str(if i == 0 { " WHERE " } else { " AND " });
        where_sql.push_str(col);
        where_sql.push_str(" = ?");
    }

    let select = format!("SELECT id FROM {table}{where_sql}");
    // Audited: `table` and every `col` are `&'static str` at each callsite;
    // the scope values are bound below.
    let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(select));
    for (_, val) in scope {
        q = q.bind((*val).to_string());
    }
    let present: Vec<String> = q
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("list {table} ids in scope for prune"))?;

    let gone: Vec<String> = present
        .into_iter()
        .filter(|id| !keep.contains(id))
        .collect();

    for chunk in gone.chunks(crate::bulk::SQL_CHUNK) {
        let mut placeholders = String::new();
        crate::bulk::push_placeholder_list(&mut placeholders, chunk.len());
        for sql in [
            format!("DELETE FROM {table} WHERE id IN ({placeholders})"),
            format!("DELETE FROM {table}_bookkeeping WHERE id IN ({placeholders})"),
        ] {
            // Audited: `table` is a `&'static str`; the IN-list is a `?,?,?`
            // run sized from the chunk and every id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id.clone());
            }
            q.execute(&mut **tx)
                .await
                .with_context(|| format!("prune {table}"))?;
        }
        forget_problems_in_tx(tx, table, &placeholders, chunk).await?;
    }
    Ok(gone)
}

/// One record's problems, for a record upstream no longer has.
pub async fn forget_record_problems_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    id: &str,
) -> Result<()> {
    forget_problems_in_tx(tx, table, "?", std::slice::from_ref(&id.to_string())).await
}

/// A record upstream no longer has cannot fail to fetch, so its problem
/// goes with it rather than standing for good.
pub(crate) async fn forget_problems_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    placeholders: &str,
    ids: &[String],
) -> Result<()> {
    let sql =
        format!("DELETE FROM problems WHERE scope_kind = ? AND scope_key IN ({placeholders})");
    // Audited: the IN-list is a `?,?,?` run sized from `ids`; every key is
    // bound.
    let mut q =
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(datalib_problems::ScopeKind::Entity.as_str());
    for id in ids {
        q = q.bind(format!("{table}:{id}"));
    }
    q.execute(&mut **tx)
        .await
        .with_context(|| format!("forget the problems of pruned {table} rows"))?;
    Ok(())
}

/// Delete the rows of `table` whose `owner_column` is one of `owners`, their
/// bookkeeping sidecars and their fetch problems: the CAS edges of records
/// a prune removed.
/// Returns how many rows went.
///
/// `table` and `owner_column` are interpolated; callers pass trusted
/// identifiers. The owners are bound.
pub async fn delete_owned(
    pool: &SqlitePool,
    table: &str,
    owner_column: &str,
    owners: &[String],
) -> Result<u64> {
    let mut tx = pool.begin().await.context("begin delete_owned tx")?;
    let removed = delete_owned_in_tx(&mut tx, table, owner_column, owners).await?;
    tx.commit().await.context("commit delete_owned tx")?;
    Ok(removed)
}

/// [`delete_owned`] inside the caller's transaction.
pub async fn delete_owned_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    owner_column: &str,
    owners: &[String],
) -> Result<u64> {
    let mut removed = 0;
    for chunk in owners.chunks(crate::bulk::SQL_CHUNK) {
        let mut placeholders = String::new();
        crate::bulk::push_placeholder_list(&mut placeholders, chunk.len());
        let sidecar = format!(
            "DELETE FROM {table}_bookkeeping WHERE id IN \
             (SELECT id FROM {table} WHERE {owner_column} IN ({placeholders}))"
        );
        let problems = format!(
            "DELETE FROM problems WHERE scope_kind = '{entity}' AND scope_key IN \
             (SELECT '{table}:' || id FROM {table} WHERE {owner_column} IN ({placeholders}))",
            entity = datalib_problems::ScopeKind::Entity.as_str(),
        );
        let rows = format!("DELETE FROM {table} WHERE {owner_column} IN ({placeholders})");
        for (i, sql) in [sidecar, problems, rows].into_iter().enumerate() {
            // Audited: `table` and `owner_column` are `&'static str` at every
            // callsite, the scope kind a closed vocabulary's spelling; the
            // IN-list is a `?,?,?` run sized from the chunk and every owner
            // is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id.clone());
            }
            let done = q
                .execute(&mut **tx)
                .await
                .with_context(|| format!("delete owned {table}"))?;
            if i == 2 {
                removed += done.rows_affected();
            }
        }
    }
    Ok(removed)
}

/// Note that a prune took place, loudly when it was a big one.
///
/// This used to be a veto: a prune taking most of a collection was refused
/// on the theory that our own enumeration narrowing — a new page cap, a
/// changed filter, a downgraded token — looks exactly like a mass deletion.
/// The premise was right and the conclusion was wrong. Deleting a row here
/// is a commit, so the previous commit still has it: `dolt_diff_<table>`
/// names what went and `dolt_at_<table>('HEAD^1')` reads it back (see
/// `docs/dev/doltlite.md`). Nothing is lost, so there is nothing to protect
/// against by refusing.
///
/// Refusing was also worse than it looked. It left the store holding rows
/// upstream no longer has, with nothing recording the divergence, and its
/// remedy was a full re-download — a bigger hammer than the prune it
/// blocked. A version-controlled store means we can afford to act on our
/// best reading and let the history be the safety net.
///
/// What survives is the signal, because an unusually large prune really is
/// worth a look, whichever explanation turns out to be right.
pub fn record(collection: &str, held: usize, gone: usize) {
    if gone == 0 {
        return;
    }
    // A prune of most of a collection is either a real clear-out or a
    // narrowed enumeration, and this line is where someone starts telling
    // them apart.
    let mostly = held > 10 && gone * 2 > held;
    if mostly {
        tracing::warn!(
            event = "prune_large",
            collection,
            held,
            removed = gone,
            "this run deleted most of a collection. If that is not what you \
             did upstream, our enumeration may have narrowed — the old rows \
             are still in history: dolt_diff_<table> names them and \
             dolt_at_<table>('HEAD^1') reads them back"
        );
    } else {
        tracing::info!(
            event = "pruned",
            collection,
            held,
            removed = gone,
            "pruned rows the upstream no longer has"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // With its bookkeeping sidecar, because that is the shape every
    // caller has and `prune_scope` clears both. A table without one is
    // not an entity table and has no business being pruned here.
    async fn open_notes(db: &std::path::Path) -> SqlitePool {
        let ddl = [
            "CREATE TABLE IF NOT EXISTS notes (
             id TEXT PRIMARY KEY, owner TEXT NOT NULL, payload TEXT )"
                .to_string(),
            crate::doltlite_raw::bookkeeping_ddl_for("notes"),
        ];
        let slices: Vec<&str> = ddl.iter().map(String::as_str).collect();
        crate::doltlite_raw::open(db, &slices).await.unwrap()
    }

    async fn record_failed(pool: &SqlitePool, id: &str) {
        let mut tx = pool.begin().await.unwrap();
        crate::doltlite_raw::record_object_error(&mut tx, "notes", id, "HTTP 500")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn problem_keys(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// A record upstream no longer has takes its fetch problem with it,
    /// whichever way it is pruned; a kept record keeps its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pruned_record_takes_its_problem_with_it() {
        let d = tempfile::tempdir().unwrap();
        let pool = open_notes(&d.path().join("t.doltlite_db")).await;
        for (id, owner) in [("a", "x"), ("b", "x"), ("e1", "gone")] {
            sqlx::query("INSERT INTO notes (id, owner, payload) VALUES (?, ?, '{}')")
                .bind(id)
                .bind(owner)
                .execute(&pool)
                .await
                .unwrap();
            record_failed(&pool, id).await;
        }
        let keep: HashSet<String> = ["a".to_string()].into_iter().collect();
        prune_scope(&pool, "notes", &[("owner", "x")], &keep)
            .await
            .unwrap();
        delete_owned(&pool, "notes", "owner", &["gone".to_string()])
            .await
            .unwrap();
        assert_eq!(problem_keys(&pool).await, ["notes:a"]);
    }

    /// `prune_scope` must delete inside its scope and nowhere else. The
    /// scope is the whole safety story now that nothing vetoes a large
    /// prune: a scope that leaks deletes rows the caller never enumerated.
    #[tokio::test(flavor = "multi_thread")]
    async fn prune_scope_deletes_only_inside_its_scope() {
        let d = tempfile::tempdir().unwrap();
        let pool = open_notes(&d.path().join("t.doltlite_db")).await;
        for (id, owner) in [("a", "x"), ("b", "x"), ("c", "y")] {
            sqlx::query("INSERT INTO notes (id, owner, payload) VALUES (?, ?, '{}')")
                .bind(id)
                .bind(owner)
                .execute(&pool)
                .await
                .unwrap();
        }

        // Owner x was re-enumerated and only `a` came back.
        let keep: HashSet<String> = ["a".to_string()].into_iter().collect();
        let gone = prune_scope(&pool, "notes", &[("owner", "x")], &keep)
            .await
            .unwrap();
        assert_eq!(gone, vec!["b".to_string()]);

        let mut left: Vec<String> = sqlx::query_scalar("SELECT id FROM notes ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        left.sort();
        assert_eq!(
            left,
            vec!["a".to_string(), "c".to_string()],
            "`c` belongs to another owner, which this walk said nothing about",
        );
    }

    /// An empty scope is the whole table, and an empty `keep` with it would
    /// delete everything — correct, but only for a caller that really did
    /// enumerate the whole collection and get nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_scope_covers_the_table() {
        let d = tempfile::tempdir().unwrap();
        let pool = open_notes(&d.path().join("t.doltlite_db")).await;
        for id in ["a", "b"] {
            sqlx::query("INSERT INTO notes (id, owner, payload) VALUES (?, 'x', '{}')")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let keep: HashSet<String> = ["a".to_string()].into_iter().collect();
        let gone = prune_scope(&pool, "notes", &[], &keep).await.unwrap();
        assert_eq!(gone, vec!["b".to_string()]);
    }

    /// A pruned record's edges go with it, and nobody else's.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_owned_removes_only_the_named_owners_rows() {
        let d = tempfile::tempdir().unwrap();
        let pool = open_notes(&d.path().join("t.doltlite_db")).await;
        for (id, owner) in [("e1", "gone"), ("e2", "gone"), ("e3", "kept")] {
            sqlx::query("INSERT INTO notes (id, owner, payload) VALUES (?, ?, '{}')")
                .bind(id)
                .bind(owner)
                .execute(&pool)
                .await
                .unwrap();
        }
        let n = delete_owned(&pool, "notes", "owner", &["gone".to_string()])
            .await
            .unwrap();
        assert_eq!(n, 2);
        let left: Vec<String> = sqlx::query_scalar("SELECT id FROM notes")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["e3".to_string()]);
    }
}
