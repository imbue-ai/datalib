//! The one row saying which of a raw store's tables no render reads. A source
//! that stores everything in an export and renders part of it would otherwise
//! hold data a person can only find by opening the store.

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::SHARED_TABLES;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Severity, Stage};
use sqlx::SqlitePool;

/// What the machinery under a download keeps, beside [`SHARED_TABLES`], and no
/// export holds.
const THE_DOWNLOADS_OWN: &[&str] = &["ingested_files", "read_marks", "cas_objects", "coverage"];

/// The raw "table" the row's key names, for
/// [`ReadScope::Whole`](crate::processor::ReadScope::Whole).
pub const SCOPE: &str = "unrendered_tables";

/// Stored tables that hold rows and are not in `read`, with their row counts,
/// by name. What the store keeps for itself is no part of an export.
pub async fn tables_not_read(pool: &SqlitePool, read: &[&str]) -> Result<Vec<(String, i64)>> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE 'dolt_%' AND name NOT LIKE '%\\_bookkeeping' ESCAPE '\\' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("list the store's tables")?;
    let mut out = Vec::new();
    for name in names {
        if read.contains(&name.as_str())
            || SHARED_TABLES.contains(&name.as_str())
            || THE_DOWNLOADS_OWN.contains(&name.as_str())
        {
            continue;
        }
        // Audited: the name comes from `sqlite_master` and is quoted.
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\""));
        let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await
            .with_context(|| format!("count {name}"))?;
        if rows > 0 {
            out.push((name, rows));
        }
    }
    Ok(out)
}

/// The row for what [`tables_not_read`] found, naming a few of them. `None` when
/// a render reads everything.
pub fn tables_not_read_row(
    source_id: &str,
    tables: &[(String, i64)],
    render_version: u32,
) -> Option<ProblemRow> {
    const NAMED: usize = 5;
    if tables.is_empty() {
        return None;
    }
    let rows: i64 = tables.iter().map(|(_, n)| n).sum();
    let names: Vec<&str> = tables.iter().take(NAMED).map(|(t, _)| t.as_str()).collect();
    let more = match tables.len().saturating_sub(NAMED) {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    Some(ProblemRow::new(
        source_id,
        Stage::Render,
        Scope::Entity(&format!("{SCOPE}:all")),
        None,
        Outcome::Dropped,
        Problem::explained(
            Reason::DeliberateLoss,
            None,
            &format!(
                "Stored and not searchable, {rows} rows in table(s): {}{more}",
                names.join(", "),
            ),
        )
        .severity(Severity::Warning),
        Some(render_version),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table nothing reads is named with its rows; one a render reads, an
    /// empty one and the store's own are not.
    #[tokio::test]
    async fn only_a_table_holding_rows_that_nothing_reads_is_named() {
        let d = tempfile::tempdir().unwrap();
        let pool = datalib_etl::doltlite_raw::open(&d.path().join("p.doltlite_db"), &[])
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE shares (id TEXT)",
            "INSERT INTO shares VALUES ('a')",
            "CREATE TABLE education (id TEXT)",
            "INSERT INTO education VALUES ('Starfleet Academy'), ('Tactical')",
            "CREATE TABLE education_bookkeeping (id TEXT)",
            "INSERT INTO education_bookkeeping VALUES ('a')",
            "CREATE TABLE skills (id TEXT)",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let found = tables_not_read(&pool, &["shares"]).await.unwrap();
        assert_eq!(found, [("education".to_string(), 2)]);
        let row = tables_not_read_row("li", &found, 1).unwrap();
        assert_eq!(row.scope_key, "unrendered_tables:all");
        assert_eq!(row.severity, Severity::Warning);
        assert_eq!(
            row.sample,
            "Stored and not searchable, 2 rows in table(s): education"
        );
        assert!(tables_not_read_row("li", &[], 1).is_none());
        pool.close().await;
    }
}
