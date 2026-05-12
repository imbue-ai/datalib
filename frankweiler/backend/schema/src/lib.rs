//! Frankweiler schema crate — re-exports the row types generated from
//! the JSON Schemas under `//schemas/`.
//!
//! Each schema becomes a submodule:
//!   * `anthropic` — anthropic_* tables (raw provider rows)
//!   * `grid_rows` — grid_rows union table (one row per displayable entity)
//!   * `feedback`  — feedback table + discriminated surface payload
//!
//! Regenerate by running:
//!     bazelisk run //schemas:update_generated
//! (or directly with `python schemas/codegen.py <schema.json> --rust ...`).

pub mod anthropic {
    include!("generated/anthropic.rs");
}

pub mod grid_rows {
    include!("generated/grid_rows.rs");
}

pub mod feedback {
    include!("generated/feedback.rs");
}

#[cfg(test)]
mod tests {
    use super::anthropic::*;

    #[test]
    fn account_round_trips() {
        let a = Account {
            account_uuid: "u-1".into(),
            full_name: Some("Test".into()),
            email_address: None,
            first_seen_at: "2026-01-01T00:00:00Z".into(),
            last_seen_at: "2026-01-01T00:00:00Z".into(),
        };
        let s = serde_json::to_string(&a).unwrap();
        let b: Account = serde_json::from_str(&s).unwrap();
        assert_eq!(b.account_uuid, "u-1");
    }

    #[test]
    fn anthropic_tables_lists_all_six() {
        assert_eq!(super::anthropic::TABLES.len(), 6);
    }

    #[test]
    fn feedback_table_present() {
        assert_eq!(super::feedback::TABLES.len(), 1);
        assert_eq!(super::feedback::DDL.len(), 1);
        let (_, cols) = super::feedback::COLUMNS[0];
        assert!(cols.contains(&"feedback_uuid"));
        assert!(cols.contains(&"context_json"));
    }

    #[test]
    fn grid_rows_table_present() {
        assert_eq!(super::grid_rows::TABLES.len(), 1);
        assert_eq!(super::grid_rows::DDL.len(), 1);
        let (_, cols) = super::grid_rows::COLUMNS[0];
        assert!(cols.contains(&"uuid"));
        assert!(cols.contains(&"channel"));
    }
}
