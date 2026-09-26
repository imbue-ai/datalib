//! A search grouped by grid columns: the groups it falls into, each with
//! its count, and the rows of one group. The grid shows every group with
//! its true count at once, and reads a group's rows only as it is opened
//! and scrolled, the way it reads an ungrouped search.

use crate::db::build_where;
use crate::query::ParsedQuery;
use crate::search::SearchRow;

/// One step of the way into a group: a `grid_rows` column (from
/// [`crate::sort::GridColumn::sql`]) and the value its rows share, `None`
/// for the rows with none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Within {
    pub column: &'static str,
    pub value: Option<String>,
}

/// One group: its value in each grouped column, how many rows it holds,
/// and its newest row, which the grid shows the group's labels from
/// before any of its rows are read.
#[derive(Debug, Clone)]
pub struct GroupCount {
    pub values: Vec<Option<String>>,
    pub count: u64,
    pub sample: SearchRow,
}

#[derive(Debug, Clone, Default)]
pub struct Grouping {
    pub groups: Vec<GroupCount>,
    /// More groups than [`MAX_GROUPS`]: the rest are left out.
    pub truncated: bool,
    pub at: Option<String>,
}

/// The most groups one answer carries. Grouping by a column that is
/// nearly unique per row, a timestamp say, is not a grouping anyone reads.
pub const MAX_GROUPS: usize = 10_000;

/// The query's own filter, narrowed to one group: `WHERE …` or empty, and
/// the values bound to its `?`s in order.
pub fn where_within(q: &ParsedQuery, within: &[Within]) -> (String, Vec<String>) {
    let (where_sql, mut params) = build_where(q);
    let mut clauses: Vec<String> = Vec::new();
    for w in within {
        match &w.value {
            Some(v) => {
                clauses.push(format!("{} = ?", w.column));
                params.push(v.clone());
            }
            None => clauses.push(format!("{} IS NULL", w.column)),
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

/// The groups of the rows `where_sql` keeps, by `by`: each group's values
/// as text, its count, and its newest row (a bare column beside the one
/// `max()` is taken from the row the maximum came from). At most one more
/// than [`MAX_GROUPS`], so the caller can tell there were more.
pub fn group_sql(table: &str, where_sql: &str, by: &[&'static str]) -> String {
    let values: Vec<String> = by.iter().map(|c| format!("CAST({c} AS TEXT)")).collect();
    let positions: Vec<String> = (1..=by.len()).map(|i| i.to_string()).collect();
    format!(
        "SELECT {}, count(*), uuid, max(touched_at_utc) FROM {table}{where_sql} GROUP BY {} LIMIT {}",
        values.join(", "),
        positions.join(", "),
        MAX_GROUPS + 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;

    #[test]
    fn a_group_narrows_the_query_and_a_missing_value_is_null() {
        let within = [
            Within {
                column: "kind",
                value: Some("Chat".into()),
            },
            Within {
                column: "author",
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
    fn the_groups_come_by_position_with_a_sample_and_a_bound() {
        assert_eq!(
            group_sql("grid_rows", " WHERE x = ?", &["kind", "author"]),
            format!(
                "SELECT CAST(kind AS TEXT), CAST(author AS TEXT), count(*), uuid, max(touched_at_utc) \
                 FROM grid_rows WHERE x = ? GROUP BY 1, 2 LIMIT {}",
                MAX_GROUPS + 1
            )
        );
    }
}
