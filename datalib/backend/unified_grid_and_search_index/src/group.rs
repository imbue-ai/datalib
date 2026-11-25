//! A search grouped by grid columns: the groups it falls into, each with
//! its count, and the rows of one group. The grid shows every group with
//! its true count at once, and reads a group's rows only as it is opened
//! and scrolled, the way it reads an ungrouped search.

use datalib_query::table::{Column, SearchTable};
use datalib_schema::grid_rows::GridRowColumn;

use crate::db::build_where;
use crate::query::ParsedQuery;
use crate::search::SearchRow;

/// One step of the way into a group: the column it groups by (a
/// column's `SearchTable::sorts_by`) and the value its rows share, `None`
/// for the rows with none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Within<C = GridRowColumn> {
    pub column: C,
    pub value: Option<String>,
}

/// One group: its value in each grouped column, how many rows it holds,
/// and its newest row, which the grid shows the group's labels from
/// before any of its rows are read.
#[derive(Debug, Clone)]
pub struct GroupCount<R = SearchRow> {
    pub values: Vec<Option<String>>,
    pub count: u64,
    pub sample: R,
}

#[derive(Debug, Clone)]
pub struct Grouping<R = SearchRow> {
    pub groups: Vec<GroupCount<R>>,
    /// More groups than [`MAX_GROUPS`]: the rest are left out.
    pub truncated: bool,
    pub at: Option<String>,
}

impl<R> Default for Grouping<R> {
    fn default() -> Self {
        Grouping {
            groups: Vec::new(),
            truncated: false,
            at: None,
        }
    }
}

/// The most groups one answer carries. Grouping by a column that is
/// nearly unique per row, a timestamp say, is not a grouping anyone reads.
pub const MAX_GROUPS: usize = 10_000;

/// The query's own filter, narrowed to one group: `WHERE …` or empty, and
/// the values bound to its `?`s in order.
pub fn where_within<C: Column>(q: &ParsedQuery<C>, within: &[Within<C>]) -> (String, Vec<String>) {
    let (where_sql, mut params) = build_where(q);
    let mut clauses: Vec<String> = Vec::new();
    for w in within {
        let column = w.column.as_str();
        match &w.value {
            Some(v) => {
                clauses.push(format!("{column} = ?"));
                params.push(v.clone());
            }
            None => clauses.push(format!("{column} IS NULL")),
        }
    }
    if clauses.is_empty() {
        return (where_sql, params);
    }
    let joiner = if where_sql.is_empty() {
        " WHERE "
    } else {
        " AND "
    };
    (
        format!("{where_sql}{joiner}{}", clauses.join(" AND ")),
        params,
    )
}

/// The groups of the rows `where_sql` keeps in `table` (the pinned name
/// of `C`'s table), by `by`: each group's values as text, its count, and
/// its newest row by the table's own order (a bare column beside the one
/// `max()` is taken from the row the maximum came from). At most one more
/// than [`MAX_GROUPS`], so the caller can tell there were more.
pub fn group_sql<C: Column>(table: &str, where_sql: &str, by: &[C]) -> String {
    let values: Vec<String> = by
        .iter()
        .map(|c| format!("CAST({} AS TEXT)", c.as_str()))
        .collect();
    let positions: Vec<String> = (1..=by.len()).map(|i| i.to_string()).collect();
    let (newest, _) = <C::Table as SearchTable>::ORDER[0];
    format!(
        "SELECT {}, count(*), {}, max({}) FROM {table}{where_sql} GROUP BY {} LIMIT {}",
        values.join(", "),
        <C::Table as SearchTable>::PRIMARY_KEY.as_str(),
        newest.as_str(),
        positions.join(", "),
        MAX_GROUPS + 1
    )
}

/// The most values one suggestion answer carries.
pub const MAX_VALUES: usize = 20;

/// The values `column` takes among the rows `where_sql` keeps in `table`,
/// those holding one bound `LIKE` pattern ([`like_pattern`]), most rows
/// first: each as text, with its count.
pub fn values_sql<C: Column>(table: &str, where_sql: &str, column: C) -> String {
    let col = column.as_str();
    let joiner = if where_sql.is_empty() {
        " WHERE"
    } else {
        " AND"
    };
    format!(
        "SELECT CAST({col} AS TEXT), count(*) FROM {table}{where_sql}{joiner} \
         {col} IS NOT NULL AND {col} != '' AND LOWER(CAST({col} AS TEXT)) LIKE ? ESCAPE '\\' \
         GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT {MAX_VALUES}"
    )
}

/// `typed` as a case-blind `LIKE` pattern for a value holding it, its own
/// `%`, `_` and `\` matched as themselves.
pub fn like_pattern(typed: &str) -> String {
    let escaped = typed
        .to_lowercase()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;

    #[test]
    fn a_group_narrows_the_query_and_a_missing_value_is_null() {
        let within = [
            Within {
                column: GridRowColumn::Kind,
                value: Some("Chat".into()),
            },
            Within {
                column: GridRowColumn::Author,
                value: None,
            },
        ];
        let (sql, params) = where_within(&parse_query(""), &within);
        assert_eq!(sql, " WHERE kind = ? AND author IS NULL");
        assert_eq!(params, ["Chat"]);

        let (sql, params) = where_within(&parse_query("channel:bridge"), &within[..1]);
        assert_eq!(sql, " WHERE channel = ? AND kind = ?");
        assert_eq!(params, ["bridge", "Chat"]);

        let (sql, params) = where_within(&parse_query("channel:bridge"), &[]);
        assert_eq!(sql, " WHERE channel = ?");
        assert_eq!(params, ["bridge"]);
    }

    #[test]
    fn values_hold_what_was_typed_most_rows_first() {
        assert_eq!(
            values_sql("grid_rows", " WHERE kind = ?", GridRowColumn::Channel),
            format!(
                "SELECT CAST(channel AS TEXT), count(*) FROM grid_rows WHERE kind = ? AND \
                 channel IS NOT NULL AND channel != '' AND LOWER(CAST(channel AS TEXT)) LIKE ? \
                 ESCAPE '\\' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT {MAX_VALUES}"
            )
        );
        assert!(
            values_sql("grid_rows", "", GridRowColumn::Channel).contains("grid_rows WHERE channel")
        );
        assert_eq!(like_pattern("Eng_50%"), "%eng\\_50\\%%");
        assert_eq!(like_pattern(""), "%%");
    }

    #[test]
    fn the_groups_come_by_position_with_a_sample_and_a_bound() {
        assert_eq!(
            group_sql(
                "grid_rows",
                " WHERE x = ?",
                &[GridRowColumn::Kind, GridRowColumn::Author]
            ),
            format!(
                "SELECT CAST(kind AS TEXT), CAST(author AS TEXT), count(*), uuid, max(touched_at_utc) \
                 FROM grid_rows WHERE x = ? GROUP BY 1, 2 LIMIT {}",
                MAX_GROUPS + 1
            )
        );
    }
}
