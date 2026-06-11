//! Shared building blocks for chunked multi-row INSERT / UPSERT
//! against doltlite raw stores.
//!
//! See `docs/data_architecture_ingestion.md` § "Bulk-upsert as the
//! standard write path" for the principle this module enforces:
//! every provider's extract goes through one entity-pool tx + one
//! CAS-pool tx per batch, each containing chunked multi-row
//! `INSERT ... ON CONFLICT(id) DO UPDATE` statements.
//!
//! The per-table entity UPSERT itself stays in the provider (each
//! provider's table has its own column list and ON CONFLICT
//! semantics). What lives here is the cross-provider plumbing:
//!
//!   - [`SQL_CHUNK`] and [`push_placeholders`] / [`push_placeholder_list`]
//!     — utilities for building chunked multi-row statements.
//!   - [`bulk_upsert_bookkeeping`] — bumps `<table>_bookkeeping`
//!     rows for a list of ids inside an open tx. Mirror of the per-row
//!     [`crate::doltlite_raw::record_object_attempt`] for the
//!     bulk-success case.
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
