//! Frankweiler **render schema** crate — the "universal schema" for the
//! denormalized tables that back the grid / UI.
//!
//! Each module is one hand-written row struct whose `CREATE TABLE` DDL
//! and column metadata are derived from the struct by
//! `#[derive(PortableTable)]` (see `frankweiler_etl_macros`). The struct
//! is the single source of truth — there is no code generation step.
//!
//!   * `grid_rows` — the union table (one row per displayable entity)
//!   * `edges`     — directed links between rendered documents / anchors
//!   * `markdowns` — per-rendered-`.md` metadata + render bookkeeping
//!
//! App-state tables that are *not* part of the render schema
//! (`feedback`, `sync_jobs`) live in the separate `app_schema` crate.

pub mod grid_rows {
    include!("grid_rows.rs");
    // Hand-written validating builder for the `GridRow` struct above.
    include!("grid_rows_builder.rs");
}

pub mod edges {
    include!("edges.rs");
}

pub mod markdowns {
    include!("markdowns.rs");
}

#[cfg(test)]
mod tests {
    #[test]
    fn grid_rows_table_present() {
        assert_eq!(super::grid_rows::TABLES.len(), 1);
        assert_eq!(super::grid_rows::DDL.len(), 1);
        let (_, cols) = super::grid_rows::COLUMNS[0];
        assert!(cols.contains(&"uuid"));
        assert!(cols.contains(&"channel"));
        // The two load-time-derived columns are present in the DDL /
        // COLUMNS metadata even though they are absent from the struct.
        assert!(cols.contains(&"when_ts_utc"));
        assert!(cols.contains(&"when_offset"));
    }

    #[test]
    fn edges_table_present() {
        assert_eq!(super::edges::TABLES.len(), 1);
        assert_eq!(super::edges::DDL.len(), 1);
        let (_, cols) = super::edges::COLUMNS[0];
        assert!(cols.contains(&"edge_uuid"));
        assert!(cols.contains(&"src_markdown_uuid"));
        assert!(cols.contains(&"dst_markdown_uuid"));
    }

    #[test]
    fn markdowns_table_present() {
        assert_eq!(super::markdowns::TABLES.len(), 1);
        assert_eq!(super::markdowns::DDL.len(), 1);
        let (_, cols) = super::markdowns::COLUMNS[0];
        assert!(cols.contains(&"markdown_uuid"));
        assert!(cols.contains(&"row_set_hash"));
    }
}
