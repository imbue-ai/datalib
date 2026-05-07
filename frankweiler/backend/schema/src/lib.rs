//! Frankweiler schema crate — re-exports the row types generated from
//! `//schemas/anthropic.schema.json`.
//!
//! Regenerate the included file with:
//!     bazelisk run //schemas:update_generated
//! (or directly: `python schemas/codegen.py schemas/anthropic.schema.json
//! --rust frankweiler/backend/schema/src/generated/anthropic.rs`)

include!("generated/anthropic.rs");

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tables_lists_all_six() {
        assert_eq!(TABLES.len(), 6);
    }
}
