//! Folder discovery and the IMAP→JMAP label vocabulary.
//!
//! Two jobs, both about making an IMAP account look like the raw schema
//! the JMAP and mbox paths already write.
//!
//! ## Finding the one folder that holds everything
//!
//! Gmail's `[Gmail]/All Mail` holds exactly one copy of every message;
//! every per-label folder is a view onto it. Mirroring per-folder would
//! download a message once per label it carries — straight into the
//! 2500 MB/day cap. So we select the all-mail folder and read label
//! membership from `X-GM-LABELS` instead.
//!
//! We find it by the RFC 6154 `\All` special-use attribute rather than
//! by name, because Gmail *localizes* the display name: a French account
//! calls it `[Gmail]/Tous les messages`. Matching on `\All` works under
//! any UI language. Servers with no `\All` fall back to mirroring the
//! folders the config names (or all of them).
//!
//! ## Labels
//!
//! `X-GM-LABELS` returns system labels backslash-prefixed (`\Inbox`,
//! `\Sent`, `\Starred`) alongside user labels as plain nested paths
//! (`Work/Projects`). Google Takeout's `X-Gmail-Labels` header spells
//! the same concepts *without* the backslash and in the UI's words
//! (`Inbox`, `Starred`, `Opened`). Both have to land on the same JMAP
//! roles and keywords, or the same account ingested through the two
//! paths would produce two different mailbox trees.

use async_imap::imap_proto::NameAttribute;

/// One folder as the server described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The server's own name, as it must be spelled back in `SELECT`.
    pub name: String,
    /// The `Parent/Child` label path, i.e. `name` with the server's
    /// hierarchy delimiter rewritten to `/`.
    pub path: String,
    /// JMAP role derived from the RFC 6154 special-use attribute.
    pub role: Option<&'static str>,
    /// `\All` — this folder holds one copy of every message.
    pub is_all: bool,
    /// `\Noselect` — a hierarchy placeholder (Gmail's bare `[Gmail]`),
    /// not something `SELECT` will open.
    pub selectable: bool,
}

/// Rewrite a server mailbox name into the `Parent/Child` label path that
/// `mailbox_labels.rs` matches `only_extract_labels` against.
///
/// IMAP servers pick their own hierarchy delimiter — `/` on Gmail, `.`
/// on many Dovecot/Courier setups. A `/` inside a segment when the
/// delimiter is `.` would otherwise read as a level of nesting that
/// isn't there, so it is escaped out of the way first.
pub fn label_path(name: &str, delimiter: Option<&str>) -> String {
    match delimiter {
        // An empty delimiter means a flat namespace: nothing to rewrite.
        None | Some("/") | Some("") => name.to_string(),
        Some(d) => name.replace('/', "_").replace(d, "/"),
    }
}

/// Derive the JMAP role from a folder's LIST attributes.
pub fn role_of(attrs: &[NameAttribute<'_>], path: &str) -> Option<&'static str> {
    for attr in attrs {
        match attr {
            NameAttribute::Sent => return Some("sent"),
            NameAttribute::Drafts => return Some("drafts"),
            NameAttribute::Trash => return Some("trash"),
            NameAttribute::Junk => return Some("junk"),
            NameAttribute::Archive | NameAttribute::All => return Some("archive"),
            _ => {}
        }
    }
    // INBOX is the one mailbox RFC 3501 names, and it is case-insensitive
    // — but it carries no special-use attribute, so it needs its own arm.
    (path.eq_ignore_ascii_case("INBOX")).then_some("inbox")
}

impl Folder {
    pub fn from_list_entry(
        name: &str,
        delimiter: Option<&str>,
        attrs: &[NameAttribute<'_>],
    ) -> Self {
        let path = label_path(name, delimiter);
        Folder {
            role: role_of(attrs, &path),
            is_all: attrs.iter().any(|a| matches!(a, NameAttribute::All)),
            selectable: !attrs.iter().any(|a| matches!(a, NameAttribute::NoSelect)),
            name: name.to_string(),
            path,
        }
    }
}

/// Pick the folder to mirror as the canonical single copy of every
/// message: the configured override if there is one, else the `\All`
/// folder. `None` means this server has no such folder and the caller
/// should mirror folders individually.
pub fn all_mail<'a>(folders: &'a [Folder], configured: Option<&str>) -> Option<&'a Folder> {
    if let Some(want) = configured {
        // Match on either spelling so a config written against the
        // server's own name works as well as one written against the
        // normalized path.
        return folders
            .iter()
            .find(|f| f.name == want || f.path == want)
            .filter(|f| f.selectable);
    }
    folders.iter().find(|f| f.is_all && f.selectable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gmail already uses `/`, so paths pass through untouched.
    #[test]
    fn leaves_slash_delimited_names_alone() {
        assert_eq!(label_path("Work/Projects", Some("/")), "Work/Projects");
        assert_eq!(label_path("INBOX", None), "INBOX");
    }

    /// Dovecot-style `.` hierarchy becomes the same `Parent/Child` form
    /// `only_extract_labels` is written in.
    #[test]
    fn rewrites_a_dot_delimiter_to_slash() {
        assert_eq!(
            label_path("INBOX.Work.Projects", Some(".")),
            "INBOX/Work/Projects"
        );
    }

    /// With a `.` delimiter a literal `/` in a segment is not nesting;
    /// letting it through would invent a level of hierarchy.
    #[test]
    fn does_not_let_a_literal_slash_fake_nesting() {
        assert_eq!(label_path("INBOX.A/B", Some(".")), "INBOX/A_B");
    }

    fn folder(name: &str, attrs: Vec<NameAttribute<'static>>) -> Folder {
        Folder::from_list_entry(name, Some("/"), &attrs)
    }

    /// Found by attribute, not by name — Gmail localizes the display
    /// name of All Mail, so a name match would break outside English.
    #[test]
    fn finds_all_mail_by_special_use_in_any_language() {
        let folders = vec![
            folder("INBOX", vec![]),
            folder("[Gmail]/Tous les messages", vec![NameAttribute::All]),
            folder("[Gmail]/Corbeille", vec![NameAttribute::Trash]),
        ];
        assert_eq!(
            all_mail(&folders, None).map(|f| f.name.as_str()),
            Some("[Gmail]/Tous les messages")
        );
    }

    #[test]
    fn honors_a_configured_all_mail_folder() {
        let folders = vec![folder("INBOX", vec![]), folder("Archive", vec![])];
        assert_eq!(
            all_mail(&folders, Some("Archive")).map(|f| f.name.as_str()),
            Some("Archive")
        );
    }

    /// A server with no `\All` gets per-folder mirroring; saying so is
    /// the caller's cue, so this must be `None` rather than a guess.
    #[test]
    fn reports_no_all_mail_folder_when_the_server_has_none() {
        let folders = vec![
            folder("INBOX", vec![]),
            folder("Sent", vec![NameAttribute::Sent]),
        ];
        assert!(all_mail(&folders, None).is_none());
    }

    /// Gmail's bare `[Gmail]` is a `\Noselect` placeholder; SELECTing it
    /// fails, so it must never be chosen.
    #[test]
    fn never_picks_an_unselectable_placeholder() {
        let folders = vec![folder(
            "[Gmail]",
            vec![NameAttribute::NoSelect, NameAttribute::All],
        )];
        assert!(all_mail(&folders, None).is_none());
        assert!(all_mail(&folders, Some("[Gmail]")).is_none());
    }

    #[test]
    fn derives_roles_from_special_use_attributes() {
        assert_eq!(folder("INBOX", vec![]).role, Some("inbox"));
        assert_eq!(folder("inbox", vec![]).role, Some("inbox"));
        assert_eq!(
            folder("[Gmail]/Sent Mail", vec![NameAttribute::Sent]).role,
            Some("sent")
        );
        assert_eq!(folder("Work", vec![]).role, None);
    }
}
