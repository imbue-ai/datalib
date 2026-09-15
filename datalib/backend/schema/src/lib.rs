//! Datalib **render schema** crate — the "universal schema" for the
//! denormalized tables that back the grid / UI.

pub mod providers {
    include!("providers.rs");
}

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

pub mod source_cursors {
    include!("source_cursors.rs");
}

pub mod render_cursor {
    include!("render_cursor.rs");
}

pub mod render_inputs {
    include!("render_inputs.rs");
}

pub mod measurements {
    include!("measurements.rs");
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
        assert!(cols.contains(&"created_at_utc"));
        assert!(cols.contains(&"created_offset"));
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

    /// The `markdowns` DDL this struct derives is the one `grid_index`
    /// creates, so this spells it out in full: a change to the struct
    /// that nobody meant to make to the table shows up here. Three
    /// drifts had already opened up while nothing read the struct — it
    /// was missing `upstream_cursor`, it
    /// declared `title` as `VARCHAR(512)` where the table had `TEXT`,
    /// and it made `renderer_version` NOT NULL where
    /// the table allows NULL, the last of which fails a write rather
    /// than merely reading wrong. Update the expectation when the
    /// change is deliberate; a re-index is the cost.
    #[test]
    fn markdowns_ddl_is_exactly_this() {
        const EXPECTED: &str = r#"CREATE TABLE IF NOT EXISTS markdowns (
    markdown_uuid VARCHAR(96) NOT NULL,
    source_id VARCHAR(64) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    kind VARCHAR(32) NOT NULL,
    title TEXT,
    created_at VARCHAR(40),
    updated_at VARCHAR(40),
    md_path VARCHAR(1024),
    upstream_cursor VARCHAR(64),
    renderer_version VARCHAR(32),
    bucket_key VARCHAR(256),
    PRIMARY KEY (markdown_uuid)
)"#;
        let derived = super::markdowns::DDL[0].1;
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            norm(derived),
            norm(EXPECTED),
            "\nderived: {derived}\n\nexpected: {EXPECTED}"
        );
    }

    /// Nothing on `markdowns` may be a per-run stamp: a re-render of an
    /// unchanged document must write an identical row, or doltlite sees
    /// a change and every consumer re-reads it. `rendered_at_utc` was
    /// exactly that, and `source_fingerprint` a second answer to a
    /// question doltlite already answers.
    #[test]
    fn markdowns_carries_no_per_run_stamp() {
        let (_, cols) = super::markdowns::COLUMNS[0];
        for gone in ["rendered_at_utc", "tz_offset", "source_fingerprint"] {
            assert!(
                !cols.contains(&gone),
                "{gone} is back on markdowns: {cols:?}"
            );
        }
        assert!(cols.contains(&"upstream_cursor"), "{cols:?}");
        assert!(cols.contains(&"bucket_key"), "{cols:?}");
    }

    #[test]
    fn markdowns_table_present() {
        assert_eq!(super::markdowns::TABLES.len(), 1);
        assert_eq!(super::markdowns::DDL.len(), 1);
        let (_, cols) = super::markdowns::COLUMNS[0];
        assert!(cols.contains(&"markdown_uuid"));
    }
}
