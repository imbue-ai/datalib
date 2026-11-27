//! `at:<node>`: the rows directly under a node of the tree, those whose
//! nearest place it is, and the document the node is, when it is one. Not
//! what is in the places below it. Three things put a row there, each read
//! off one covering index, and [`Arms`] says which of them this node has:
//! a search `OR`s the ones it has, and only an `OR` of two makes SQLite
//! read each row's table page.

/// What puts a row directly under a node.
use datalib_sql::{sql, Sql};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arms {
    /// It lives there: a channel's threads and their messages.
    pub held: bool,
    /// It is in what is filed there: a label's threads.
    pub filed: bool,
    /// It is in the document the node is: a page, a chat, a thread.
    pub own: bool,
}

impl Arms {
    /// Any of them, until [`FACTS_SQL`] says.
    pub const UNREAD: Arms = Arms {
        held: true,
        filed: true,
        own: true,
    };
}

/// [`Arms`] of the node bound as `?1`.
pub const FACTS_SQL: Sql = sql!(
    "unified_grid_and_search_index.find_node_arms",
    "SELECT \
     EXISTS (SELECT 1 FROM grid_rows INDEXED BY grid_rows_by_place WHERE place_uuid = ?1), \
     EXISTS (SELECT 1 FROM filings INDEXED BY filings_by_under WHERE under_uuid = ?1), \
     EXISTS (SELECT 1 FROM grid_rows WHERE uuid = ?1 AND is_document = 1)"
);

/// The rows at `node`, as one clause over `grid_rows` and the values it
/// binds.
pub fn clause(node: &str, arms: Arms, negate: bool) -> (String, Vec<String>) {
    let any: Vec<&str> = [
        (arms.held, "place_uuid = ?"),
        (
            arms.filed,
            "conversation_uuid IN (SELECT node_uuid FROM filings INDEXED BY filings_by_under \
             WHERE under_uuid = ?)",
        ),
        (
            arms.own,
            "conversation_uuid IN (SELECT uuid FROM grid_rows WHERE uuid = ? AND is_document = 1)",
        ),
    ]
    .into_iter()
    .filter_map(|(has, sql)| has.then_some(sql))
    .collect();
    let clause = match any.as_slice() {
        [] => "0".to_string(),
        [one] => (*one).to_string(),
        _ => format!("({})", any.join(" OR ")),
    };
    // A row in no place compares as null, which is not at the node.
    let clause = if negate {
        format!("NOT COALESCE({clause}, 0)")
    } else {
        clause
    };
    (clause, vec![node.to_string(); any.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node with one way to hold rows asks of one index alone; one with
    /// none keeps nothing.
    #[test]
    fn a_clause_asks_only_of_the_arms_a_node_has() {
        let none = Arms {
            held: false,
            filed: false,
            own: false,
        };
        assert_eq!(clause("n", none, false), ("0".to_string(), vec![]));
        assert_eq!(
            clause("n", Arms { held: true, ..none }, false),
            ("place_uuid = ?".to_string(), vec!["n".to_string()])
        );
        let (sql, bound) = clause("n", Arms::UNREAD, true);
        assert!(sql.starts_with("NOT COALESCE((place_uuid = ? OR "), "{sql}");
        assert_eq!(sql.matches('?').count(), bound.len());
        assert_eq!(bound.len(), 3);
    }
}
