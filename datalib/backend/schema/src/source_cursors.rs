// How far the unified index has consumed each source's render store.
//
// One row per source, living in the index database — the *consumer*
// owns its cursor, not the producer. That placement is what makes the
// two impossible to disagree: wipe the index and the cursors go with
// it, so the next run rebuilds from nothing rather than skipping past
// documents the index no longer holds. It is also why this table is
// listed in `grid_index::index_ddl` alongside `grid_rows` /
// `markdowns` / `edges`: the schema reconcile drops and rebuilds that
// whole set together, and a cursor that outlived the rows it points
// past would silently freeze the index.
//
// The value is a doltlite commit hash from the source's own
// `indexed_markdown.doltlite_db`. Asking that store
// `dolt_diff_<table>(from_ref = <cursor>, to_ref = 'HEAD')` is what
// replaces reading every document and comparing fingerprints in Rust —
// see `datalib_etl::indexed_markdown::IndexedMarkdownStore::changed_since`.
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One source's index cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "source_cursors", primary_key = "source_name")]
pub struct SourceCursorRow {
    /// The stanza / config-level source name — the same value
    /// `markdowns.source_name` carries.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: String,
    /// The render store's `dolt_log()` HEAD at the moment the index
    /// finished consuming it. The next run passes this as `from_ref`.
    ///
    /// Written inside the index's own write transaction, so it advances
    /// if and only if the rows it accounts for landed. A crash between
    /// the two would otherwise strand the index a run behind its cursor
    /// — and re-applying a document is harmless (delete-then-insert
    /// keyed on `markdown_uuid`) where skipping one is not.
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
