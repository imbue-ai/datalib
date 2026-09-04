//! Shared building blocks for chunked multi-row INSERT / UPSERT
//! against doltlite raw stores.
//!
//! See `docs/dev/data_architecture_ingestion.md` §"One writer per row"
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
//!   - [`BulkUpsertable`] — the row-struct contract.
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
        // Audited: only `bk_table` (= `{table}_bookkeeping`) is interpolated, and
        // the VALUES run is `push_placeholders` over `chunk.len()`. All bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
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
/// The row-struct write contract.
///
/// Defined in [`datalib_schema::bulk`] and re-exported here, because
/// `datalib_etl` depends on `datalib_schema` — so the render-schema
/// structs could not implement a trait that lived in this crate. Every
/// existing `datalib_etl::bulk::BulkUpsertable` path keeps working
/// through this re-export.
pub use datalib_schema::bulk::BulkUpsertable;

/// Generic bulk-UPSERT for any [`BulkUpsertable`] row type. The one
/// entity-table write path every provider should use.
///
/// Runs **inside an open `tx`** so the caller can batch multiple
/// table upserts atomically. Per-batch behavior:
///
///   1. Chunks `rows` at [`SQL_CHUNK`] rows per statement.
///   2. For each chunk, emits one
///      `INSERT INTO <T::TABLE> (id, <typed_cols>, <payload>) VALUES
///       (?,…,jsonb(?)),(?,…,jsonb(?)),… ON CONFLICT(id) DO UPDATE
///       SET <every non-id col> = excluded.<col>`.
///   3. After all chunks land, stamps `<T::TABLE>_bookkeeping` for
///      every id via [`bulk_upsert_bookkeeping`] in the same tx.
///
/// The caller commits `tx`. No-op if `rows` is empty.
pub async fn bulk_upsert_in_tx<T: BulkUpsertable>(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[T],
    now: &str,
) -> Result<()> {
    bulk_upsert_entity_in_tx(tx, rows).await?;
    if rows.is_empty() {
        return Ok(());
    }
    bulk_upsert_bookkeeping(tx, T::TABLE, rows.iter().map(|r| r.id()), now).await
}

/// The entity-table half of [`bulk_upsert_in_tx`], WITHOUT the paired
/// `<t>_bookkeeping` stamp. Use this for tables that deliberately have
/// no bookkeeping sidecar — e.g. `datalib-etl-fsindex`, where the
/// sidecars (a) aren't needed (the scanner has no retry/attempt model)
/// and (b) roughly double the row count, which matters at the
/// tens-of-millions-of-rows design scale. The framework default is
/// still [`bulk_upsert_in_tx`] (always-paired bookkeeping); opting out
/// is a deliberate per-provider choice.
pub async fn bulk_upsert_entity_in_tx<T: BulkUpsertable>(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[T],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let table = T::TABLE;

    // Column lists: typed columns first, then (optionally) payload.
    let mut cols_csv = String::new();
    for (i, c) in T::TYPED_COLUMNS.iter().enumerate() {
        if i > 0 {
            cols_csv.push_str(", ");
        }
        cols_csv.push_str(c);
    }
    if let Some(payload_col) = T::PAYLOAD_COLUMN {
        if !T::TYPED_COLUMNS.is_empty() {
            cols_csv.push_str(", ");
        }
        cols_csv.push_str(payload_col);
    }

    // ON CONFLICT SET clause — every non-id col gets excluded.<col>
    // per §"One writer per row" in the ingestion doc.
    let mut set_csv = String::new();
    for c in T::TYPED_COLUMNS {
        if !set_csv.is_empty() {
            set_csv.push_str(", ");
        }
        set_csv.push_str(&format!("{c} = excluded.{c}"));
    }
    if let Some(payload_col) = T::PAYLOAD_COLUMN {
        if !set_csv.is_empty() {
            set_csv.push_str(", ");
        }
        set_csv.push_str(&format!("{payload_col} = excluded.{payload_col}"));
    }

    // VALUES tuple: id and typed columns as plain `?`, payload (if
    // present) as `jsonb(?)`.
    let mut tuple = String::from("(?");
    for _ in T::TYPED_COLUMNS {
        tuple.push_str(",?");
    }
    if T::PAYLOAD_COLUMN.is_some() {
        tuple.push_str(",jsonb(?)");
    }
    tuple.push(')');

    let id_col = T::ID_COLUMN;
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = format!("INSERT INTO {table} ({id_col}, {cols_csv}) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&tuple);
        }
        sql.push_str(&format!(" ON CONFLICT({id_col}) DO UPDATE SET "));
        sql.push_str(&set_csv);

        // Audited: `table`, `id_col`, `cols_csv` and `set_csv` all derive from the
        // `BulkUpsert` impl's associated consts, not from row data; the per-row
        // `tuple` is a `(?,?,?)` run. Every value is bound by `bind_into`.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for row in chunk {
            q = row.bind_into(q);
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("bulk_upsert {table}"))?;
    }
    // Per-table upsert tally for the current source's download metrics
    // (no-op outside an download scope). Every generic entity write —
    // `bulk_upsert_in_tx` and slack's `bulk_upsert_with_tape` alike —
    // funnels through here, so this is the one place that needs to know.
    crate::download_metrics::record_upserts(table, rows.len());
    Ok(())
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
