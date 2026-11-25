// The data nodes themselves, one row each: what every reader of the tree
// reads. A document says of each node it sits in what it knows
// (`data_nodes`), so a channel has a row there under every thread; the
// index step settles those into one row here. In the grid index alone.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What listing a container's children reads, made in the grid index
/// alone: nothing asks a render store for children. A page of children is
/// one range of a `(parent, sort_key, uuid)` index, read in order.
pub const CHILDREN_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS grid_rows_by_parent ON grid_rows (parent_uuid, sort_key, uuid)",
    // `source_id` last, so a source's top is read off the index too.
    "CREATE INDEX IF NOT EXISTS nodes_places ON nodes (parent_uuid, sort_key, uuid, source_id) \
     WHERE is_place = 1",
    "CREATE INDEX IF NOT EXISTS filings_by_under ON filings (under_uuid, node_uuid)",
];

/// What `at:<node>` reads a place's rows off, newest first and from the
/// index alone, as the indexes of the search's own keys are. In the grid
/// index alone.
pub const PLACE_INDEX: &str = "CREATE INDEX IF NOT EXISTS grid_rows_by_place \
     ON grid_rows (place_uuid, touched_at_utc, is_document, uuid, source_id)";

/// A data node, its columns as on [`super::data_nodes::DataNodeRow`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "nodes", primary_key = "uuid")]
pub struct NodeRow {
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    #[col(sql = "VARCHAR(64)")]
    pub kind: String,
    #[col(sql = "TEXT")]
    pub name: Option<String>,
    #[col(sql = "VARCHAR(96)")]
    pub parent_uuid: Option<String>,
    #[col(sql = "VARCHAR(40)")]
    pub sort_key: String,
    #[col(sql = "INTEGER")]
    pub read_together: bool,
    #[col(sql = "INTEGER")]
    pub is_place: bool,
    #[col(sql = "VARCHAR(96)")]
    pub read_uuid: Option<String>,
    #[col(sql = "VARCHAR(96)")]
    pub place_uuid: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub upstream_entity_kind: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub upstream_id: Option<String>,
    #[col(sql = "VARCHAR(256)")]
    pub upstream_account: Option<String>,
}

#[cfg(test)]
mod tests {
    /// A node says everything a document says of one, and nothing else: a
    /// column added to `data_nodes` is added here.
    #[test]
    fn a_node_has_every_column_a_documents_account_has_but_the_document() {
        let (_, account) = crate::data_nodes::COLUMNS[0];
        let wanted: Vec<&str> = account
            .iter()
            .copied()
            .filter(|c| *c != "markdown_uuid")
            .collect();
        let (_, node) = super::COLUMNS[0];
        assert_eq!(node, wanted);
    }
}
