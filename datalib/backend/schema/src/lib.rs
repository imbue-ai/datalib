//! Datalib **render schema** crate — the "universal schema" for the
//! denormalized tables that back the grid / UI.
//!
//! Each module is one hand-written row struct whose `CREATE TABLE` DDL
//! and column metadata are derived from the struct by
//! `#[derive(PortableTable)]` (see `datalib_etl_macros`). The struct
//! is the single source of truth — there is no code generation step.
//!
//!   * `grid_rows` — the union table (one row per displayable entity)
//!   * `edges`     — directed links between rendered documents / anchors
//!   * `markdowns` — per-rendered-`.md` metadata + render bookkeeping
//!   * `render_problems` — what render could not do, beside what it did
//!
//! App-state tables that are *not* part of the render schema
//! (`feedback`, `sync_jobs`) live in the separate `app_schema` crate.

// So the `PortableTable` derive can emit `impl
// ::datalib_schema::bulk::BulkUpsertable` for structs defined *inside*
// this crate: without the self-alias that absolute path does not
// resolve here, and a relative one would not resolve in the provider
// crates that also use the derive.
extern crate self as datalib_schema;

pub mod bulk;

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

pub mod render_problems {
    include!("render_problems.rs");
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
    fn render_problems_table_present() {
        assert_eq!(super::render_problems::TABLES.len(), 1);
        assert_eq!(super::render_problems::DDL.len(), 1);
        let (_, cols) = super::render_problems::COLUMNS[0];
        for want in ["uuid", "scope_key", "scope_kind", "outcome", "problems"] {
            assert!(cols.contains(&want), "missing {want}: {cols:?}");
        }
    }

    /// The `markdowns` DDL this struct derives is what `grid_index`
    /// creates, so it must stay byte-equal to the string that lived
    /// there before the two were consolidated. Three drifts had already
    /// opened up while nothing read the struct: it was missing
    /// `source_fingerprint` and `upstream_cursor`, it declared `title`
    /// as `VARCHAR(512)` where production had `TEXT`, and it made
    /// `row_set_hash` / `renderer_version` NOT NULL where production
    /// allows NULL — the last of which fails a write rather than
    /// merely reading wrong.
    #[test]
    fn markdowns_ddl_matches_what_the_index_has_always_created() {
        const PRODUCTION: &str = r#"CREATE TABLE IF NOT EXISTS markdowns (
    markdown_uuid VARCHAR(96) NOT NULL,
    source_name VARCHAR(64) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    kind VARCHAR(32) NOT NULL,
    title TEXT,
    created_at VARCHAR(40),
    updated_at VARCHAR(40),
    md_path VARCHAR(1024),
    source_fingerprint VARCHAR(64),
    upstream_cursor VARCHAR(64),
    row_set_hash CHAR(64),
    renderer_version VARCHAR(32),
    rendered_at VARCHAR(40),
    PRIMARY KEY (markdown_uuid)
)"#;
        let derived = super::markdowns::DDL[0].1;
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            norm(derived),
            norm(PRODUCTION),
            "\nderived: {derived}\n\nproduction: {PRODUCTION}"
        );
    }

    /// The columns `grid_index`'s hand-written `MARKDOWNS_DDL` writes
    /// have to be on the struct, or the struct is not the schema.
    /// `source_fingerprint` and `upstream_cursor` were absent for as
    /// long as `MarkdownRow` went unused by anything.
    #[test]
    fn markdowns_covers_the_render_bookkeeping_columns() {
        let (_, cols) = super::markdowns::COLUMNS[0];
        assert!(cols.contains(&"source_fingerprint"), "{cols:?}");
        assert!(cols.contains(&"upstream_cursor"), "{cols:?}");
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
