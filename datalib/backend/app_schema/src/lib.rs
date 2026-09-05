//! Datalib **app-state schema** crate — the tables that hold the
//! application's own state rather than rendered/presentable data.

pub mod feedback {
    include!("feedback.rs");
}

pub mod sync_jobs {
    include!("sync_jobs.rs");
}

pub mod disk_usage {
    include!("disk_usage.rs");
}

#[cfg(test)]
mod tests {
    #[test]
    fn feedback_table_present() {
        assert_eq!(super::feedback::TABLES.len(), 1);
        assert_eq!(super::feedback::DDL.len(), 1);
        let (_, cols) = super::feedback::COLUMNS[0];
        assert!(cols.contains(&"feedback_uuid"));
        assert!(cols.contains(&"context_json"));
    }

    #[test]
    fn sync_jobs_table_present() {
        assert_eq!(super::sync_jobs::TABLES.len(), 1);
        assert_eq!(super::sync_jobs::DDL.len(), 1);
        let (_, cols) = super::sync_jobs::COLUMNS[0];
        assert!(cols.contains(&"id"));
        assert!(cols.contains(&"state"));
    }

    /// The disk-usage timeseries is keyed on (series, instant): one
    /// series is many rows, and the pair is what makes each unique.
    /// A single-column key would silently collapse the history to its
    /// newest sample.
    #[test]
    fn disk_usage_is_keyed_on_path_and_instant() {
        assert_eq!(super::disk_usage::TABLES.len(), 1);
        let (_, cols) = super::disk_usage::COLUMNS[0];
        assert!(cols.contains(&"path"));
        assert!(cols.contains(&"measured_at"));
        assert!(cols.contains(&"bytes"));
        let (_, ddl) = super::disk_usage::DDL[0];
        assert!(
            ddl.contains("PRIMARY KEY (path, measured_at)"),
            "expected a composite key, got: {ddl}"
        );
    }
}
