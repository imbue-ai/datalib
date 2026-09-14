// How far a source's render has consumed its raw store. One row per
// render store, written in the same transaction as the last document of
// the run it describes, so it can never claim more than the store holds.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "render_cursor", primary_key = "source_id")]
pub struct RenderCursorRow {
    /// The source's id — its group. The store belongs to one source, so
    /// this is the one row; it is keyed so `dolt_diff` can read it.
    #[col(sql = "VARCHAR(64)")]
    pub source_id: String,
    /// The raw store's commit the last render consumed. The next run
    /// passes it as `from_ref` to the `dolt_diff` scan.
    #[col(sql = "VARCHAR(64)")]
    pub raw_commit: String,
    /// The render params (JSON) the documents were rendered with. A run
    /// whose params differ renders every bucket again and keeps
    /// `raw_commit` for the diff.
    #[col(sql = "TEXT")]
    pub params: String,
    /// When this cursor was last advanced (ISO-8601 with explicit
    /// offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub rendered_at: String,
}
