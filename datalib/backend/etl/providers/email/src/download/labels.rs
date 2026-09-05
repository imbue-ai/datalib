//! The one label vocabulary, shared by every download mode.

use sha2::{Digest, Sha256};

/// How one label maps into the raw schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelMap {
    /// A `mailboxes` row, with an optional JMAP role.
    Mailbox { role: Option<&'static str> },
    /// A JMAP keyword on the email, not a mailbox of its own.
    Keyword(&'static str),
    /// Explicitly *not* seen — modeled as the absence of `$seen`.
    Unread,
    /// Carries no information we store.
    Drop,
}

/// System labels, keyed by their lowercased, backslash-stripped,
/// underscore-normalized spelling. The canonical name is Takeout's.
const SYSTEM: &[(&str, &str, LabelKind)] = &[
    ("inbox", "Inbox", LabelKind::Mailbox(Some("inbox"))),
    ("sent", "Sent", LabelKind::Mailbox(Some("sent"))),
    ("draft", "Drafts", LabelKind::Mailbox(Some("drafts"))),
    ("drafts", "Drafts", LabelKind::Mailbox(Some("drafts"))),
    ("trash", "Trash", LabelKind::Mailbox(Some("trash"))),
    ("spam", "Spam", LabelKind::Mailbox(Some("junk"))),
    ("junk", "Spam", LabelKind::Mailbox(Some("junk"))),
    ("all mail", "All Mail", LabelKind::Mailbox(Some("archive"))),
    ("important", "Important", LabelKind::Keyword("$important")),
    ("starred", "Starred", LabelKind::Keyword("$flagged")),
    ("flagged", "Starred", LabelKind::Keyword("$flagged")),
    ("opened", "Opened", LabelKind::Keyword("$seen")),
    ("read", "Opened", LabelKind::Keyword("$seen")),
    ("unread", "Unread", LabelKind::Unread),
    // "Archived" in Takeout means *absence* of Inbox, not a label; Gmail's
    // `\Muted` has no JMAP counterpart.
    ("archived", "Archived", LabelKind::Drop),
    ("muted", "Muted", LabelKind::Drop),
    // Gmail's inbox categories. Takeout spells them "Category Promotions";
    // the API spells them CATEGORY_PROMOTIONS.
    (
        "category personal",
        "Category Personal",
        LabelKind::Mailbox(None),
    ),
    (
        "category social",
        "Category Social",
        LabelKind::Mailbox(None),
    ),
    (
        "category promotions",
        "Category Promotions",
        LabelKind::Mailbox(None),
    ),
    (
        "category updates",
        "Category Updates",
        LabelKind::Mailbox(None),
    ),
    (
        "category forums",
        "Category Forums",
        LabelKind::Mailbox(None),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    Mailbox(Option<&'static str>),
    Keyword(&'static str),
    Unread,
    Drop,
}

impl LabelKind {
    fn into_map(self) -> LabelMap {
        match self {
            LabelKind::Mailbox(role) => LabelMap::Mailbox { role },
            LabelKind::Keyword(k) => LabelMap::Keyword(k),
            LabelKind::Unread => LabelMap::Unread,
            LabelKind::Drop => LabelMap::Drop,
        }
    }
}

/// Reduce a label to the key the [`SYSTEM`] table is indexed by:
/// lowercase, `_` treated as a space so the Gmail API's
/// `CATEGORY_PROMOTIONS` lands on the same entry as Takeout's
/// `Category Promotions`, and a leading backslash stripped — Google spells
/// system labels that way on some surfaces, and normalization is the right
/// place to be liberal about it.
fn lookup_key(label: &str) -> String {
    label
        .trim()
        .strip_prefix('\\')
        .unwrap_or(label.trim())
        .replace('_', " ")
        .to_ascii_lowercase()
}

fn system_entry(label: &str) -> Option<(&'static str, LabelKind)> {
    let key = lookup_key(label);
    SYSTEM
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, canonical, kind)| (*canonical, *kind))
}

/// The spelling to store for `label`, whatever spelling it arrived in.
///
/// User labels pass through untouched — their name *is* the user's, and
/// all three modes report it identically (`Work/Projects`).
pub fn canonical_name(label: &str) -> String {
    match system_entry(label) {
        Some((canonical, _)) => canonical.to_string(),
        None => label.trim().to_string(),
    }
}

pub fn map_label(label: &str) -> LabelMap {
    match system_entry(label) {
        Some((_, kind)) => kind.into_map(),
        None => LabelMap::Mailbox { role: None },
    }
}

pub fn mailbox_id(account_id: &str, label: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"mbox:");
    h.update(account_id.as_bytes());
    h.update(b":");
    h.update(label.trim().as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(29);
    out.push_str("mbox-");
    for b in digest.iter().take(12) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

pub fn split_gmail_labels(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                cur.push(next);
                chars.next();
            }
            continue;
        }
        if c == ',' {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists for: every spelling of one
    /// Gmail label must produce one mailbox row with one id.
    #[test]
    fn all_spellings_of_a_system_label_agree() {
        for (takeout, backslashed, api) in [
            ("Inbox", "\\Inbox", "INBOX"),
            ("Sent", "\\Sent", "SENT"),
            ("Trash", "\\Trash", "TRASH"),
            ("Spam", "\\Spam", "SPAM"),
            ("Starred", "\\Starred", "STARRED"),
            ("Important", "\\Important", "IMPORTANT"),
            (
                "Category Promotions",
                "Category Promotions",
                "CATEGORY_PROMOTIONS",
            ),
        ] {
            assert_eq!(
                canonical_name(takeout),
                canonical_name(backslashed),
                "{takeout}"
            );
            assert_eq!(canonical_name(takeout), canonical_name(api), "{takeout}");
            assert_eq!(map_label(takeout), map_label(backslashed), "{takeout}");
            assert_eq!(map_label(takeout), map_label(api), "{takeout}");
            // Canonicalize first — that is the documented contract of
            // `mailbox_id`, and the step every mode performs for a label
            // Google marked as a system one.
            assert_eq!(
                mailbox_id("acct", &canonical_name(takeout)),
                mailbox_id("acct", &canonical_name(api)),
                "{takeout} and {api} would produce two mailbox rows",
            );
        }
    }

    /// Every label the checked-in Takeout fixture actually carries must
    /// canonicalize to itself — that is what makes this refactor a no-op
    /// for raw stores already on disk (and why `jmap_mbox` still passes).
    #[test]
    fn leaves_the_takeout_fixtures_own_labels_untouched() {
        for label in [
            "Inbox",
            "Sent",
            "Starred",
            "Important",
            "Unread",
            "Category Promotions",
        ] {
            assert_eq!(canonical_name(label), label, "{label} would be rewritten");
        }
    }

    #[test]
    fn maps_system_labels_to_roles_and_keywords() {
        assert_eq!(
            map_label("INBOX"),
            LabelMap::Mailbox {
                role: Some("inbox")
            }
        );
        assert_eq!(map_label("\\Starred"), LabelMap::Keyword("$flagged"));
        assert_eq!(map_label("UNREAD"), LabelMap::Unread);
        assert_eq!(map_label("\\Muted"), LabelMap::Drop);
        assert_eq!(map_label("Archived"), LabelMap::Drop);
    }

    /// Both Gmail spellings of the drafts folder land on one role and one
    /// canonical name, so `DRAFT` and a Takeout `Drafts` don't fork.
    #[test]
    fn folds_the_two_drafts_spellings_together() {
        assert_eq!(canonical_name("DRAFT"), canonical_name("Drafts"));
        assert_eq!(
            map_label("DRAFT"),
            LabelMap::Mailbox {
                role: Some("drafts")
            }
        );
    }

    #[test]
    fn keeps_user_labels_verbatim_and_roleless() {
        assert_eq!(canonical_name("Work/Projects"), "Work/Projects");
        assert_eq!(map_label("Work/Projects"), LabelMap::Mailbox { role: None });
        // Nesting under a system-sounding word is still a user label.
        assert_eq!(canonical_name("Archive/Inbox"), "Archive/Inbox");
        assert_eq!(map_label("Archive/Inbox"), LabelMap::Mailbox { role: None });
    }

    /// Different accounts must not share mailbox ids, or two mirrors in
    /// one store would collide.
    #[test]
    fn scopes_mailbox_ids_to_the_account() {
        assert_ne!(mailbox_id("a@x", "Inbox"), mailbox_id("b@x", "Inbox"));
        assert!(mailbox_id("a@x", "Inbox").starts_with("mbox-"));
    }

    /// Gmail lets a user create a label literally named `INBOX`. Folding
    /// it onto the system inbox would merge two distinct mailboxes, so
    /// `mailbox_id` must take the name it is given and nothing else.
    #[test]
    fn does_not_canonicalize_behind_the_callers_back() {
        assert_ne!(mailbox_id("acct", "INBOX"), mailbox_id("acct", "Inbox"));
        assert_eq!(
            mailbox_id("acct", &canonical_name("INBOX")),
            mailbox_id("acct", "Inbox"),
        );
    }

    #[test]
    fn splits_escaped_commas_in_a_takeout_header() {
        assert_eq!(
            split_gmail_labels("Inbox,Work\\, Urgent,Starred"),
            vec!["Inbox", "Work, Urgent", "Starred"]
        );
        assert!(split_gmail_labels("").is_empty());
    }
}
