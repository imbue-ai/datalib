//! `at:<node>`: the rows directly under a node of the tree, those whose
//! nearest place it is, and the document the node is, when it is one. Not
//! what is in the places below it. Three things put a row there, each read
//! off one covering index, and [`Arms`] says which of them this node has:
//! a search asks for the ones it has, and only where a node has two are
//! their rows found first and then fetched from the table.

/// What puts a row directly under a node.
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
pub const FACTS_SQL: &str = "SELECT \
     EXISTS (SELECT 1 FROM grid_rows INDEXED BY grid_rows_by_place WHERE place_uuid = ?1), \
     EXISTS (SELECT 1 FROM filings INDEXED BY filings_by_under WHERE under_uuid = ?1), \
     EXISTS (SELECT 1 FROM grid_rows WHERE uuid = ?1 AND is_document = 1)";

/// The rows at `node`, as one clause over `grid_rows` and the values it
/// binds.
pub fn clause(node: &str, arms: Arms, negate: bool) -> (String, Vec<String>) {
    let any: Vec<(&str, &str)> = [
        (arms.held, "grid_rows_by_place", "place_uuid = ?"),
        (
            arms.filed,
            "grid_rows_by_conversation",
            "conversation_uuid IN (SELECT node_uuid FROM filings INDEXED BY filings_by_under \
             WHERE under_uuid = ?)",
        ),
        (
            arms.own,
            "grid_rows_by_conversation",
            "conversation_uuid = (SELECT uuid FROM grid_rows WHERE uuid = ? AND is_document = 1)",
        ),
    ]
    .into_iter()
    .filter_map(|(has, index, sql)| has.then_some((index, sql)))
    .collect();
    let clause = match any.as_slice() {
        [] => "0".to_string(),
        [(_, one)] => (*one).to_string(),
        // Not an `OR`: beside `is:document` or a source, SQLite serves one
        // side of it from that one's index, and reads all of it.
        _ => {
            let sets: Vec<String> = any
                .iter()
                .map(|(index, sql)| {
                    format!("SELECT rowid FROM grid_rows INDEXED BY {index} WHERE {sql}")
                })
                .collect();
            format!("rowid IN ({})", sets.join(" UNION "))
        }
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
        assert!(
            sql.starts_with("NOT COALESCE(rowid IN (SELECT rowid "),
            "{sql}"
        );
        assert_eq!(sql.matches(" UNION ").count(), 2, "{sql}");
        assert_eq!(sql.matches('?').count(), bound.len());
        assert_eq!(bound.len(), 3);
    }
}
