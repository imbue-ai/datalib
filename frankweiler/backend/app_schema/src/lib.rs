//! Frankweiler **app-state schema** crate — the tables that hold the
//! application's own state rather than rendered/presentable data.
//!
//! These tables are *not* part of the render schema (`frankweiler_schema`,
//! which defines `grid_rows` / `edges` / `markdowns`). They were split out
//! so the render pipeline's "universal schema" no longer has reach into
//! UI feedback and the background job queue:
//!
//!   * `feedback`   — user-filed feedback on datalib surfaces
//!   * `sync_jobs`  — background job queue for UI-driven sync
//!
//! Each module is a hand-written row struct whose `CREATE TABLE` DDL and
//! column metadata are derived from the struct by
//! `#[derive(PortableTable)]` (see `frankweiler_etl_macros`). The struct
//! is the single source of truth — there is no code generation step.

pub mod feedback {
    include!("feedback.rs");
}

pub mod sync_jobs {
    include!("sync_jobs.rs");
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
}
