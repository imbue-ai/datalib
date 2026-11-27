//! A multi-row statement over a slice of rows: `head VALUES (?, …), (?, …)
//! tail`, as many rows to a statement as SQLite binds, so a writer spends
//! a statement on many rows rather than one on each.

use sqlx::query::Query;
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Sqlite, SqliteConnection};

use crate::{Sql, SqlId};

/// The most `?` one statement may bind: SQLite's compile-time default,
/// which doltlite keeps.
pub const MAX_VARIABLES: usize = 32_766;

/// The most rows one statement writes, however narrow they are: enough
/// that a page of documents costs a handful of statements, few enough that
/// each one prepares quickly.
pub const MAX_ROWS: usize = 500;

/// Runs `head VALUES … tail` over `rows`, `columns` values a row, each
/// row's bound by `bind` in order; what every statement returned, which is
/// nothing without a `RETURNING`. SQLite does not promise the order of
/// what a multi-row statement returns, so read it as a set or by a key it
/// carries.
pub async fn fetch<'r, R>(
    conn: &mut SqliteConnection,
    id: SqlId,
    head: &str,
    columns: usize,
    tail: &str,
    rows: &'r [R],
    bind: impl Fn(Query<'r, Sqlite, SqliteArguments>, &'r R) -> Query<'r, Sqlite, SqliteArguments>,
) -> Result<Vec<SqliteRow>, sqlx::Error> {
    let tuple = format!("({})", vec!["?"; columns].join(", "));
    let per_statement = (MAX_VARIABLES / columns.max(1)).clamp(1, MAX_ROWS);
    let mut returned = Vec::new();
    for chunk in rows.chunks(per_statement) {
        let tuples = vec![tuple.as_str(); chunk.len()].join(", ");
        // Audited: the caller's constant text around a run of placeholders
        // sized from the chunk; every value is bound.
        let mut q = Sql::dynamic(id, format!("{head} VALUES {tuples} {tail}")).query();
        for row in chunk {
            q = bind(q, row);
        }
        returned.extend(q.fetch_all(&mut *conn).await?);
    }
    Ok(returned)
}
