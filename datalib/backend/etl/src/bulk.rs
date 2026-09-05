//! Shared building blocks for chunked multi-row INSERT / UPSERT
//! against doltlite raw stores.
//!
//! One UPSERT shape for every entity table — see the crate README, and
//! [`insert_sql`] for the one path that deliberately does not upsert.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

/// One table's worth of `(id, payload)` pairs to record in a single
/// bulk-write batch. Shared by the entity-side
/// [`crate::doltlite_raw::bulk_upsert_events`] chokepoint (where the
/// payload may be ignored — only the id drives bookkeeping) and the
/// tape-side [`crate::event_tape::EventTape::append_batch`] mirror
/// (where the payload becomes the JSONL line).
pub struct EventBatch<'a> {
    pub table: &'a str,
    pub rows: &'a [(&'a str, &'a Value)],
}

/// Default rows per multi-row `INSERT` statement. Well under SQLite's
/// 32k parameter ceiling for typical entity-row widths (e.g. 10 cols
/// at this chunk size ⇒ 4000 binds per statement). Callers writing
/// unusually wide rows should chunk smaller.
pub const SQL_CHUNK: usize = 400;

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

pub fn push_placeholder_list(sql: &mut String, count: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
}

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
pub use datalib_schema::bulk::BulkUpsertable;

pub fn insert_sql<T: BulkUpsertable>() -> String {
    let mut cols = String::from(T::ID_COLUMN);
    for c in T::TYPED_COLUMNS {
        cols.push_str(", ");
        cols.push_str(c);
    }
    let n = 1 + T::TYPED_COLUMNS.len();
    let mut placeholders = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            placeholders.push_str(", ");
        }
        placeholders.push('?');
    }
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        T::TABLE,
        cols,
        placeholders
    )
}

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
