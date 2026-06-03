//! vCard → grid_rows + rendered markdown.
//!
//! Translate reads `.vcf` files from a directory tree of the shape
//!   `<input_path>/<addressbook_dir>/<some_name>.vcf`
//! and emits one rendered markdown file + one grid row per contact.
//! The directory name (the addressbook label) becomes the `channel`
//! column on each row so the UI can group all contacts in one
//! addressbook together.
//!
//! This path also works for the test pipeline: a config that omits
//! the `sync:` block is translate-only (same shape as
//! `SourceConfig::ClaudeExport`), so a checked-in fixture full of
//! vCards renders without any CardDAV server in the loop.
//!
//! The UUID derivation is upstream-stable: `contact_uuid(account,
//! addressbook, uid)` derives the same id whether the vCard came
//! over CardDAV or off disk.

pub mod parse;
pub mod render;

use std::sync::OnceLock;

use uuid::Uuid;

/// Stable namespace for every UUIDv5 derivation in this provider.
/// Picked once + frozen so re-ingests are idempotent across
/// machines.
pub fn contacts_uuid_ns() -> &'static Uuid {
    static NS: OnceLock<Uuid> = OnceLock::new();
    NS.get_or_init(|| {
        Uuid::parse_str("3f4c6e9a-7c2b-4f1d-8b5a-1c2d3e4f5a6b").expect("valid contacts uuid ns")
    })
}

/// PK derivation for a contact across the whole stack: vCards from
/// the same UID under the same `(account, addressbook)` collapse
/// into the same row, no matter whether they came from a
/// sync-collection REPORT or a `.vcf` file on disk.
pub fn contact_uuid(account_id: &str, addressbook_label: &str, uid: &str) -> String {
    let name = format!("contact:{account_id}:{addressbook_label}:{uid}");
    Uuid::new_v5(contacts_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Stable UUID for an addressbook. Used as the `conversation_uuid`
/// on every grid row so the UI groups all contacts in one
/// addressbook together.
pub fn addressbook_uuid(account_id: &str, addressbook_label: &str) -> String {
    let name = format!("addressbook:{account_id}:{addressbook_label}");
    Uuid::new_v5(contacts_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_uuid_is_stable() {
        let a = contact_uuid("contacts.icloud.com", "Personal", "uid-1");
        let b = contact_uuid("contacts.icloud.com", "Personal", "uid-1");
        assert_eq!(a, b);
        let c = contact_uuid("contacts.icloud.com", "Work", "uid-1");
        assert_ne!(a, c);
    }
}
