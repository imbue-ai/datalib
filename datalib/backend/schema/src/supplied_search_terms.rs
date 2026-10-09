// The search terms a render supplies for its rows
// (`crate::search_terms::SuppliedSearchTerm`), kept with the document that
// supplied them the way its edges are: a re-render replaces its own rows.
// In every render store and in the grid index, which the search terms file
// is built from.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "supplied_search_terms", primary_key = "uuid, kind, value")]
pub struct SuppliedSearchTermRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// The grid row the term is for.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    /// A `SearchTermKind`, spelled.
    #[col(sql = "VARCHAR(32)")]
    pub kind: String,
    #[col(sql = "TEXT")]
    pub value: String,
}
