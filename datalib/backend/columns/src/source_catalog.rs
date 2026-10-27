//! What a source type is called and which mark it gets, resolved before
//! the row is sent — by whichever producer serves rows about sources:
//! the Manage rows and the search grid both. The wizard's descriptors —
//! fields, pickers, probes — stay in the browser
//! (`ui/src/config/catalog.ts`); this is only the part a row needs. A
//! type with variants (`email` is Gmail or Fastmail by the method table
//! on its ingest step) is narrowed by which table the params carry.

use crate::Identity;

struct Entry {
    r#type: &'static str,
    /// The params key that selects this variant; `None` is the type's
    /// plain entry, which any params match.
    variant: Option<&'static str>,
    label: &'static str,
    icon: Option<&'static str>,
}

// Every icon is a file's name in `datalib/ui/src/assets/`, and each label
// and icon matches the browser's catalog for the same type
// (`scripts/lint_repo.py`, check 13).
const CATALOG: &[Entry] = &[
    e("slack", None, "Slack", Some("slack")),
    e("claude", Some("api"), "Claude", Some("claude")),
    e("claude", Some("export"), "Claude (export)", Some("claude")),
    e("claude_code", None, "Claude Code", Some("claude_code")),
    e("chatgpt", None, "ChatGPT", Some("chatgpt")),
    e(
        "chatgpt",
        Some("export"),
        "ChatGPT (export)",
        Some("chatgpt"),
    ),
    e("codex", None, "Codex", Some("codex")),
    e("github", None, "GitHub", Some("github")),
    e("gitlab", None, "GitLab", Some("gitlab")),
    e("notion", None, "Notion", Some("notion")),
    e("email", Some("gmail"), "Gmail", Some("gmail")),
    e("email", Some("jmap"), "Fastmail", Some("fastmail")),
    e("email", None, "Email (mbox or JMAP)", Some("email")),
    e(
        "calendar",
        Some("google"),
        "Google Calendar",
        Some("google_calendar"),
    ),
    e(
        "calendar",
        Some("fastmail"),
        "Fastmail Calendar",
        Some("fastmail_calendar"),
    ),
    e(
        "calendar",
        Some("caldav"),
        "Calendar (CalDAV)",
        Some("calendar"),
    ),
    e(
        "calendar",
        Some("ics"),
        "Calendar (.ics files)",
        Some("calendar"),
    ),
    e("calendar", None, "Calendar", Some("calendar")),
    e(
        "contacts",
        Some("fastmail"),
        "Fastmail Contacts",
        Some("fastmail_contacts"),
    ),
    e(
        "contacts",
        Some("carddav"),
        "Contacts (CardDAV)",
        Some("contacts"),
    ),
    e(
        "contacts",
        Some("vcf"),
        "Contacts (.vcf files)",
        Some("contacts"),
    ),
    e("contacts", None, "Contacts", Some("contacts")),
    e("garmin", None, "Garmin", Some("garmin")),
    e("yolink", None, "YoLink", Some("yolink")),
    e("google_drive", None, "Google Drive", Some("google_drive")),
    e(
        "google_takeout",
        None,
        "Google Takeout",
        Some("google_takeout"),
    ),
    e("linkedin", None, "LinkedIn", Some("linkedin")),
    e("facebook", None, "Facebook", Some("facebook")),
    e("signal", None, "Signal backups", Some("signal")),
    e("whatsapp", None, "WhatsApp backups", Some("whatsapp")),
    e(
        "sms_backup_restore",
        None,
        "SMS Backup & Restore",
        Some("sms"),
    ),
    e("beeper", None, "Beeper", Some("beeper")),
    e("pdf", None, "Local PDFs", Some("pdf")),
    e("fsindex", None, "Local file index", Some("fsindex")),
    e("media", None, "Local media", Some("media")),
    e("airvisual", None, "IQAir AirVisual", Some("airvisual")),
    e("lightroom", None, "Lightroom", Some("lightroom")),
    e("linear", None, "Linear", Some("linear")),
    e("apple_photos", None, "Apple Photos", Some("apple_photos")),
    e(
        "apple_messages",
        None,
        "Apple Messages",
        Some("apple_messages"),
    ),
    e("perseus", None, "Perseus Digital Library", Some("perseus")),
    e("zoom", None, "Zoom", Some("zoom")),
    // Not a provider: a group that renders what changed in another
    // group's raw store between two commits (docs/dev/plans/completed/diff_renderer.md).
    e("diff", None, "Diff", Some("diff")),
];

const fn e(
    r#type: &'static str,
    variant: Option<&'static str>,
    label: &'static str,
    icon: Option<&'static str>,
) -> Entry {
    Entry {
        r#type,
        variant,
        label,
        icon,
    }
}

/// The identity of a source type, narrowed by the ingest step's params
/// when the type has variants. A type the catalog does not know is
/// shown by its own name, with no mark.
pub fn source_type(r#type: &str, params: &serde_json::Value) -> Identity {
    let candidates = CATALOG.iter().filter(|c| c.r#type == r#type);
    // The variant the params select; else the type's plain entry; else,
    // for a type that is only variants, its first — a step with no
    // method table yet is still that kind of source.
    let found = candidates
        .clone()
        .find(|c| c.variant.is_some_and(|v| params.get(v).is_some()))
        .or_else(|| candidates.clone().find(|c| c.variant.is_none()))
        .or_else(|| candidates.clone().next());
    match found {
        Some(c) => Identity {
            id: r#type.to_string(),
            label: c.label.to_string(),
            icon: c.icon.map(str::to_string),
            detail: None,
            entity: None,
        },
        None => Identity {
            id: r#type.to_string(),
            label: r#type.to_string(),
            icon: None,
            detail: None,
            entity: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_variant_is_picked_by_the_method_table_the_params_carry() {
        assert_eq!(source_type("email", &json!({"gmail": {}})).label, "Gmail");
        assert_eq!(source_type("email", &json!({"jmap": {}})).label, "Fastmail");
        assert_eq!(
            source_type("email", &json!({"mbox": {}})).label,
            "Email (mbox or JMAP)"
        );
        assert_eq!(source_type("claude", &json!({})).label, "Claude");
        assert_eq!(
            source_type("claude", &json!({"export": {"path": "x"}})).label,
            "Claude (export)"
        );
        assert_eq!(source_type("chatgpt", &json!({"api": {}})).label, "ChatGPT");
        assert_eq!(
            source_type("chatgpt", &json!({"export": {"path": "x"}})).label,
            "ChatGPT (export)"
        );
        assert_eq!(
            source_type("contacts", &json!({"fastmail": {}})).label,
            "Fastmail Contacts"
        );
        assert_eq!(
            source_type("contacts", &json!({"carddav": {"server_url": "x"}})).label,
            "Contacts (CardDAV)"
        );
        assert_eq!(
            source_type("contacts", &json!({"vcf": {"path": "x"}})).label,
            "Contacts (.vcf files)"
        );
        assert_eq!(source_type("contacts", &json!({})).label, "Contacts");
    }

    /// The Manage screen has no Type column; a group's mark is the only
    /// sign of what it mirrors, so a type without one is unmarked there.
    #[test]
    fn every_known_type_has_a_mark() {
        let unmarked: Vec<&str> = CATALOG
            .iter()
            .filter(|c| c.icon.is_none())
            .map(|c| c.label)
            .collect();
        assert_eq!(unmarked, Vec::<&str>::new());
    }

    #[test]
    fn an_unknown_type_is_its_own_name_with_no_mark() {
        let got = source_type("hologram", &json!({}));
        assert_eq!(got.label, "hologram");
        assert_eq!(got.icon, None);
    }
}
