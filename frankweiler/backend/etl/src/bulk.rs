//! Shared building blocks for chunked multi-row INSERT / UPSERT
//! against doltlite raw stores.
//!
//! See `docs/data_architecture_ingestion.md` §"One writer per row"
//! and §"Bulk-upsert as the standard write path" for the principle
//! this module enforces:
//!
//!   - **Every entity table** uses the same UPSERT shape:
//!     `INSERT INTO <t> (id, …cols) VALUES (...)  ON CONFLICT(id)
//!     DO UPDATE SET <every non-id col> = excluded.<col>`. No
//!     `COALESCE`-style per-column policies; each write is complete.
//!   - **Provider code** declares its row struct and a [`BulkUpsertable`]
//!     impl next to the DDL constant in `schema_raw.rs`, then calls
//!     the generic [`bulk_upsert_in_tx`] helper to write a batch.
//!     There should be no provider-side hand-written bulk UPSERT SQL.
//!
//! Module surface:
//!
//!   - [`BulkUpsertable`] + [`ColumnKind`] — the row-struct contract.
//!   - [`bulk_upsert_in_tx`] — the one generic UPSERT helper.
//!   - [`SQL_CHUNK`], [`push_placeholders`], [`push_placeholder_list`]
//!     — chunking utilities the helper uses (and which a few
//!     transitional callsites still touch directly).
//!   - [`bulk_upsert_bookkeeping`] — bumps `<table>_bookkeeping`
//!     rows for a list of ids inside an open tx. Mirror of the per-row
//!     [`crate::doltlite_raw::record_object_attempt`] for the
//!     bulk-success case. Called from inside [`bulk_upsert_in_tx`];
//!     also exposed for transitional callsites that aren't yet on
//!     the trait.
//!
//! The chokepoint that pairs entity-side UPSERT bookkeeping with the
//! post-commit JSONL wire-tape append lives in
//! [`crate::doltlite_raw::bulk_upsert_events`].

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::{Sqlite, Transaction};

/// One table's worth of `(id, payload)` pairs to record in a single
/// bulk-write batch. Shared by the entity-side
/// [`crate::doltlite_raw::bulk_upsert_events`] chokepoint (where the
/// payload may be ignored — only the id drives bookkeeping) and the
/// tape-side [`crate::event_tape::EventTape::append_batch`] mirror
/// (where the payload becomes the JSONL line).
///
/// Lives here in the bulk module rather than alongside the tape
/// because it is the primary load-bearing shape; the tape is a
/// best-effort sidecar built on top.
pub struct EventBatch<'a> {
    pub table: &'a str,
    pub rows: &'a [(&'a str, &'a Value)],
}

/// Default rows per multi-row `INSERT` statement. Well under SQLite's
/// 32k parameter ceiling for typical entity-row widths (e.g. 10 cols
/// at this chunk size ⇒ 4000 binds per statement). Callers writing
/// unusually wide rows should chunk smaller.
pub const SQL_CHUNK: usize = 400;

/// Push `count` copies of `(?, ?, …)` (each tuple has `cols` placeholders),
/// comma-separated. Used to construct the VALUES list for a chunked
/// multi-row INSERT.
pub fn push_placeholders(sql: &mut String, count: usize, cols: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('(');
        for j in 0..cols {
            if j > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');
    }
}

/// Push `count` comma-separated `?` placeholders (no surrounding
/// parens). Used for `WHERE id IN (?, ?, …)` lists.
pub fn push_placeholder_list(sql: &mut String, count: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
}

/// Bulk-upsert one row into `<table>_bookkeeping` per id, stamping
/// `fetched_at = now`, `attempt_count += 1`, `last_error = NULL`.
/// No-op if `ids` is empty.
///
/// This is the success-side bulk counterpart to the per-row
/// [`crate::doltlite_raw::record_object_attempt`]. Use it after the
/// matching entity-table INSERT inside the same tx.
pub async fn bulk_upsert_bookkeeping<'a, I>(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    ids: I,
    now: &str,
) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    let ids: Vec<&str> = ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let bk_table = format!("{table}_bookkeeping");
    for chunk in ids.chunks(SQL_CHUNK) {
        let mut sql = format!(
            "INSERT INTO {bk_table} (id, fetched_at, attempt_count, last_attempt_at, last_error) VALUES "
        );
        push_placeholders(&mut sql, chunk.len(), 5);
        sql.push_str(&format!(
            " ON CONFLICT(id) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                attempt_count = {bk_table}.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL"
        ));
        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q
                .bind(*id)
                .bind(now)
                .bind(1_i64)
                .bind(now)
                .bind::<Option<&str>>(None);
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("bulk_upsert_bookkeeping {bk_table}"))?;
    }
    Ok(())
}

/// How a column's value should be bound in the multi-row INSERT.
///
/// Two shapes cover every entity table in the workspace:
///
/// - [`ColumnKind::Plain`] — placeholder `?`. The bound value's
///   sqlx `Encode` impl handles the rest. Use for typed columns
///   (`TEXT`, `INTEGER`, `BOOLEAN`, …) and for **raw-bytes payloads**
///   like Signal's protobuf `BLOB`s.
/// - [`ColumnKind::Jsonb`] — placeholder `jsonb(?)`. The bound value
///   is a JSON text string; SQLite's `jsonb()` function converts it
///   to the on-disk JSONB encoding. Use for any `payload` column
///   that stores JSON.
///
/// Either kind is opaque to the trait; only the generic helper looks
/// at it (to emit `?` or `jsonb(?)` in the VALUES tuple).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Plain,
    Jsonb,
}

/// Row-struct contract that lets the generic [`bulk_upsert_in_tx`]
/// helper write a batch into a table.
///
/// **Where impls live.** By convention, the row struct and its
/// `BulkUpsertable` impl live in the provider's `schema_raw.rs`,
/// right next to the matching `CREATE TABLE` DDL constant, so that
/// the rust struct's fields and the SQL columns are visibly aligned
/// at the same vertical position in the file.
///
/// **Required correspondence.** [`Self::COLUMNS`] must list the
/// non-PK columns in the same order as [`Self::bind_into`] binds
/// them, and that order must match the DDL's column declarations
/// after `id`. Mismatch → mis-binding at runtime.
///
/// **One writer per row.** Per
/// `docs/data_architecture_ingestion.md` §"One writer per row," the
/// ON CONFLICT clause is uniform across all tables: every non-PK
/// column is set to `excluded.<col>`. There is no per-table or
/// per-column override.
pub trait BulkUpsertable: Sync {
    /// Target table name. Must match the DDL.
    const TABLE: &'static str;

    /// Non-PK columns + their bind shape, in bind order. The helper
    /// uses this to build the INSERT column list, the VALUES tuple
    /// placeholder, and the ON CONFLICT SET clause.
    const COLUMNS: &'static [(&'static str, ColumnKind)];

    /// PK value for this row. The PK column is always named `id` in
    /// every raw entity table (see
    /// `docs/data_architecture_ingestion.md` §"Object identity").
    fn id(&self) -> &str;

    /// Bind the id and every non-PK column value in [`Self::COLUMNS`]
    /// order. The helper has already emitted matching placeholders
    /// (`?` for `Plain`, `jsonb(?)` for `Jsonb`) in the SQL; this
    /// method just calls `q.bind(...)` once per column, starting
    /// with `id`.
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>>;
}

/// Generic bulk-UPSERT for any [`BulkUpsertable`] row type. The one
/// entity-table write path every provider should use.
///
/// Runs **inside an open `tx`** so the caller can batch multiple
/// table upserts atomically. Per-batch behavior:
///
///   1. Chunks `rows` at [`SQL_CHUNK`] rows per statement.
///   2. For each chunk, emits one `INSERT INTO <T::TABLE> (id, …)
///      VALUES (?,?,…),(?,?,…),… ON CONFLICT(id) DO UPDATE SET …`
///      with `T`'s column list and per-column placeholders driven by
///      [`ColumnKind`].
///   3. After all chunks land, stamps `<T::TABLE>_bookkeeping` for
///      every id via [`bulk_upsert_bookkeeping`] in the same tx.
///
/// The caller commits `tx`.
///
/// No-op if `rows` is empty.
pub async fn bulk_upsert_in_tx<T: BulkUpsertable>(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[T],
    now: &str,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let table = T::TABLE;
    let cols_csv = T::COLUMNS
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ");
    let set_csv = T::COLUMNS
        .iter()
        .map(|(n, _)| format!("{n} = excluded.{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    // VALUES tuple: id placeholder is always `?`; each non-PK column
    // contributes either `?` or `jsonb(?)` per its `ColumnKind`.
    let mut tuple = String::from("(?");
    for (_, kind) in T::COLUMNS {
        tuple.push(',');
        match kind {
            ColumnKind::Plain => tuple.push('?'),
            ColumnKind::Jsonb => tuple.push_str("jsonb(?)"),
        }
    }
    tuple.push(')');

    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = format!("INSERT INTO {table} (id, {cols_csv}) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&tuple);
        }
        sql.push_str(" ON CONFLICT(id) DO UPDATE SET ");
        sql.push_str(&set_csv);

        let mut q = sqlx::query(&sql);
        for row in chunk {
            q = row.bind_into(q);
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("bulk_upsert {table}"))?;
    }
    bulk_upsert_bookkeeping(tx, table, rows.iter().map(|r| r.id()), now).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_single_col() {
        let mut s = String::new();
        push_placeholders(&mut s, 3, 1);
        assert_eq!(s, "(?),(?),(?)");
    }

    #[test]
    fn placeholders_multi_col() {
        let mut s = String::new();
        push_placeholders(&mut s, 2, 3);
        assert_eq!(s, "(?,?,?),(?,?,?)");
    }

    #[test]
    fn placeholder_list_emits_bare_qs() {
        let mut s = String::new();
        push_placeholder_list(&mut s, 4);
        assert_eq!(s, "?,?,?,?");
    }

    #[test]
    fn placeholders_zero_count_is_empty() {
        let mut s = String::new();
        push_placeholders(&mut s, 0, 5);
        assert_eq!(s, "");
    }
}
