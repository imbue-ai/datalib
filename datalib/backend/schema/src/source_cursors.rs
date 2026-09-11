// How far the unified index has consumed each source's render store.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One source's index cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "source_cursors", primary_key = "source_name")]
pub struct SourceCursorRow {
    /// The source's id — its group, the same value
    /// `markdowns.source_name` carries. The column keeps that older
    /// spelling for the same reason: it is this table's primary key,
    /// and renaming it would cost a re-index.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: String,
    /// The render store's `dolt_log()` HEAD at the moment the index
    /// finished consuming it. The next run passes this as `from_ref`.
    #[col(sql = "VARCHAR(64)")]
    pub store_commit: String,
    /// When this cursor was last advanced (ISO-8601 with explicit
    /// offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub indexed_at: String,
    /// How many documents the run that advanced this cursor applied.
    /// Diagnostic only — it is what makes "the index did nothing
    /// because nothing changed" distinguishable from "the index did
    /// nothing because it was broken" when reading the table by hand.
    #[col(sql = "INT")]
    pub documents_applied: i64,
}
