//! One Gmail API message → one `EmailRow`, plus its CAS entry.
//!
//! All of the interesting work is shared: [`super::super::envelope`]
//! builds the JMAP-shaped envelope, [`super::super::labels`] resolves the
//! label vocabulary, and the id derivation is the same `Message-ID`
//! rule every mode uses. What is left here is the Gmail-specific
//! translation into those shared shapes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use datalib_etl::blob_cas::blake3_hex;

use super::super::envelope::{self, TransportFacts};
use super::super::labels::{self, LabelMap};
use super::super::schema_raw::EmailRow;
use super::api::{GmailMessage, Label};

/// Resolved label metadata, built once per run from `users.labels.list`.
#[derive(Debug, Clone, Default)]
pub struct LabelIndex {
    by_id: BTreeMap<String, Label>,
}

impl LabelIndex {
    pub fn new(labels: Vec<Label>) -> Self {
        Self {
            by_id: labels.into_iter().map(|l| (l.id.clone(), l)).collect(),
        }
    }

    /// The display name for a label id.
    ///
    /// Falls back to the id itself for a label the list call didn't
    /// return — better a mailbox named `Label_7` than a silently dropped
    /// one, since the alternative loses the fact that the message was
    /// filed somewhere.
    fn name(&self, id: &str) -> String {
        match self.by_id.get(id) {
            Some(label) => label.name.clone(),
            None => id.to_string(),
        }
    }

    /// Whether Google called this a system label.
    ///
    /// Load-bearing: a *user* label named `INBOX` must not be folded onto
    /// the system inbox, and only this flag can tell them apart.
    fn is_system(&self, id: &str) -> bool {
        self.by_id.get(id).is_some_and(|l| l.is_system)
    }

    /// Every label that should become a `mailboxes` row, as
    /// `(mailbox_id, canonical_name, role)`.
    pub fn mailboxes(&self, account_id: &str) -> Vec<(String, String, Option<&'static str>)> {
        let mut out: BTreeMap<String, (String, Option<&'static str>)> = BTreeMap::new();
        for label in self.by_id.values() {
            let LabelMap::Mailbox { role } = self.map(&label.id) else {
                continue;
            };
            let canonical = if label.is_system {
                labels::canonical_name(&label.name)
            } else {
                label.name.clone()
            };
            out.insert(
                labels::mailbox_id(account_id, &canonical),
                (canonical, role),
            );
        }
        out.into_iter()
            .map(|(id, (name, role))| (id, name, role))
            .collect()
    }

    fn map(&self, id: &str) -> LabelMap {
        let name = self.name(id);
        if self.is_system(id) {
            labels::map_label(&name)
        } else {
            // A user label is a mailbox keeping its own name, whatever it
            // happens to be spelled like.
            LabelMap::Mailbox { role: None }
        }
    }

    /// Split a message's `labelIds` into mailbox ids and JMAP keywords.
    pub fn resolve(&self, account_id: &str, label_ids: &[String]) -> (Vec<String>, Vec<String>) {
        let mut mailbox_ids: Vec<String> = Vec::new();
        let mut keywords: BTreeSet<String> = BTreeSet::new();
        let mut is_unread = false;
        for id in label_ids {
            let name = self.name(id);
            match self.map(id) {
                LabelMap::Mailbox { .. } => {
                    let canonical = if self.is_system(id) {
                        labels::canonical_name(&name)
                    } else {
                        name
                    };
                    let mid = labels::mailbox_id(account_id, &canonical);
                    if !mailbox_ids.contains(&mid) {
                        mailbox_ids.push(mid);
                    }
                }
                LabelMap::Keyword(k) => {
                    keywords.insert(k.to_string());
                }
                LabelMap::Unread => is_unread = true,
                LabelMap::Drop => {}
            }
        }
        // JMAP models read-ness as the presence of `$seen`; Gmail models
        // it as the presence of `UNREAD`. Same convention the mbox path
        // uses, so the two agree.
        if !is_unread {
            keywords.insert("$seen".to_string());
        }
        (mailbox_ids, keywords.into_iter().collect())
    }

    /// Resolve configured label *names* to Gmail label *ids*, for the
    /// server-side `messages.list?labelIds=` filter.
    ///
    /// Errors on a name that matches nothing rather than silently
    /// returning an empty filter: an empty filter means "every message in
    /// the account", so a typo'd label would quietly turn a small
    /// targeted mirror into a full one.
    pub fn ids_for_names(&self, names: &[String]) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let found = self.by_id.values().find(|l| {
                let canonical = if l.is_system {
                    labels::canonical_name(&l.name)
                } else {
                    l.name.clone()
                };
                canonical == *name || l.name == *name
            });
            match found {
                Some(label) => out.push(label.id.clone()),
                None => anyhow::bail!(
                    "email `only_extract_labels` names {name:?}, which is not a label on this \
                     Gmail account. Known labels: {}",
                    self.known_names().join(", ")
                ),
            }
        }
        Ok(out)
    }

    /// Label names as a user would write them in `only_extract_labels`.
    fn known_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_id
            .values()
            .map(|l| {
                if l.is_system {
                    labels::canonical_name(&l.name)
                } else {
                    l.name.clone()
                }
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The canonical label names a message carries, for the download-time
    /// `only_extract_labels` filter. Matched against the same
    /// `Parent/Child` paths as every other mode.
    pub fn label_paths(&self, label_ids: &[String]) -> Vec<String> {
        label_ids
            .iter()
            .map(|id| {
                let name = self.name(id);
                if self.is_system(id) {
                    labels::canonical_name(&name)
                } else {
                    name
                }
            })
            .collect()
    }
}

/// Gmail thread ids are hex; Google Takeout's `X-GM-THRID` header spells
/// the same 64-bit number in decimal.
///
/// Normalizing to decimal is what lets a mailbox ingested from a Takeout
/// export and then from the API land on **one** thread per conversation
/// instead of two. Anything that isn't hex passes through unchanged.
pub fn normalize_thread_id(gmail_thread_id: &str) -> String {
    match u64::from_str_radix(gmail_thread_id, 16) {
        Ok(n) => n.to_string(),
        Err(_) => gmail_thread_id.to_string(),
    }
}

/// What one ingested message contributes.
pub struct Ingested {
    pub row: EmailRow,
    pub email_id: String,
    pub thread_id: String,
    pub received_at: String,
    pub blob_id: String,
    pub raw: Vec<u8>,
    /// Canonical label paths, for the extract-time label filter.
    pub label_paths: Vec<String>,
}

/// Translate one `messages.get?format=RAW` response into a row.
pub fn ingest(account_id: &str, index: &LabelIndex, msg: &GmailMessage) -> Result<Ingested> {
    let parsed = envelope::parse(&msg.raw)?;
    let blob_id = blake3_hex(&msg.raw);
    let email_id = envelope::email_id(&parsed, &blob_id);
    let thread_id = normalize_thread_id(&msg.thread_id);
    let (mailbox_ids, keywords) = index.resolve(account_id, &msg.label_ids);

    let facts = TransportFacts {
        email_id: email_id.clone(),
        blob_id: blob_id.clone(),
        thread_id: thread_id.clone(),
        mailbox_ids,
        keywords,
    };
    let mut env = envelope::synthesize(&msg.raw, &parsed, &facts);

    // `Date:` can be absent or unparseable; `internalDate` is Gmail's own
    // receipt time and always present, so it is the better fallback than
    // leaving the row undated (thread ordering depends on it).
    let received_at = envelope::received_at(&parsed)
        .or_else(|| msg.internal_date_ms.map(internal_date_to_iso))
        .unwrap_or_default();
    if let Some(obj) = env.as_object_mut() {
        if !received_at.is_empty() && obj.get("receivedAt").is_none() {
            obj.insert(
                "receivedAt".into(),
                serde_json::Value::String(received_at.clone()),
            );
        }
        // Provenance, mirroring what the claude provider stamps: which
        // transport produced this row, and the ids only this one knows.
        obj.insert(
            "_source".into(),
            serde_json::json!({
                "via": "gmail.googleapis.com",
                "gmailMessageId": msg.id,
                "gmailThreadId": msg.thread_id,
            }),
        );
    }

    let row = EmailRow::from_jmap_envelope(account_id, &env).ok_or_else(|| {
        anyhow::anyhow!("could not build an email row for Gmail message {}", msg.id)
    })?;
    Ok(Ingested {
        row,
        email_id,
        thread_id,
        received_at,
        blob_id,
        raw: msg.raw.clone(),
        label_paths: index.label_paths(&msg.label_ids),
    })
}

/// `internalDate` (epoch **milliseconds**) → an offset-bearing ISO-8601
/// string.
///
/// Per the repo-wide timestamp convention, an epoch number carries no
/// source offset, so it renders as UTC with an explicit `+00:00` — never
/// a bare `Z`-suffixed `strftime`.
fn internal_date_to_iso(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Utc.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.fixed_offset().to_rfc3339(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(id: &str, name: &str, system: bool) -> Label {
        Label {
            id: id.into(),
            name: name.into(),
            is_system: system,
        }
    }

    fn index() -> LabelIndex {
        LabelIndex::new(vec![
            label("INBOX", "INBOX", true),
            label("SENT", "SENT", true),
            label("UNREAD", "UNREAD", true),
            label("STARRED", "STARRED", true),
            label("CATEGORY_PROMOTIONS", "CATEGORY_PROMOTIONS", true),
            label("Label_7", "Work/Projects", false),
        ])
    }

    /// The dedup property, at the level that matters: the API's `INBOX`
    /// and Takeout's `Inbox` must resolve to the same mailbox id.
    #[test]
    fn resolves_system_labels_onto_the_shared_mailbox_ids() {
        let (mailboxes, _) = index().resolve("acct", &["INBOX".into()]);
        assert_eq!(mailboxes, vec![labels::mailbox_id("acct", "Inbox")]);
    }

    #[test]
    fn splits_labels_into_mailboxes_and_keywords() {
        let (mailboxes, keywords) = index().resolve(
            "acct",
            &["INBOX".into(), "STARRED".into(), "Label_7".into()],
        );
        assert_eq!(mailboxes.len(), 2, "STARRED is a keyword, not a mailbox");
        assert!(mailboxes.contains(&labels::mailbox_id("acct", "Inbox")));
        assert!(mailboxes.contains(&labels::mailbox_id("acct", "Work/Projects")));
        assert_eq!(keywords, vec!["$flagged", "$seen"]);
    }

    /// Gmail says "UNREAD"; JMAP says "no $seen". Getting this backwards
    /// would mark the whole mailbox read in the grid.
    #[test]
    fn models_unread_as_the_absence_of_seen() {
        let (_, read) = index().resolve("acct", &["INBOX".into()]);
        assert!(read.contains(&"$seen".to_string()));
        let (_, unread) = index().resolve("acct", &["INBOX".into(), "UNREAD".into()]);
        assert!(!unread.contains(&"$seen".to_string()));
    }

    /// A *user* label named INBOX is not the inbox. Only Google's
    /// `type: system` flag distinguishes them.
    #[test]
    fn does_not_fold_a_user_label_onto_a_system_one() {
        let idx = LabelIndex::new(vec![
            label("INBOX", "INBOX", true),
            label("Label_9", "INBOX", false),
        ]);
        let (system, _) = idx.resolve("acct", &["INBOX".into()]);
        let (user, _) = idx.resolve("acct", &["Label_9".into()]);
        assert_ne!(system, user);
    }

    #[test]
    fn resolves_configured_names_to_gmail_label_ids() {
        let idx = index();
        assert_eq!(
            idx.ids_for_names(&["Work/Projects".into()]).unwrap(),
            vec!["Label_7".to_string()]
        );
        // Written the canonical (Takeout) way, matched against Gmail's
        // ALL-CAPS system name.
        assert_eq!(idx.ids_for_names(&["Inbox".into()]).unwrap(), vec!["INBOX"]);
        // Written Gmail's way, also matched.
        assert_eq!(idx.ids_for_names(&["INBOX".into()]).unwrap(), vec!["INBOX"]);
    }

    /// An empty `labelIds` filter means "every message in the account",
    /// so a typo must fail loudly rather than quietly turning a targeted
    /// mirror into a full one.
    #[test]
    fn refuses_a_label_name_that_matches_nothing() {
        let err = index()
            .ids_for_names(&["Datalib".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("Datalib"), "{err}");
        // The message has to say what the options were.
        assert!(err.contains("Work/Projects"), "{err}");
    }

    #[test]
    fn resolves_nothing_for_an_empty_filter() {
        assert!(index().ids_for_names(&[]).unwrap().is_empty());
    }

    /// A label id the list call didn't return still files the message
    /// somewhere, rather than silently vanishing.
    #[test]
    fn keeps_an_unknown_label_as_a_mailbox() {
        let (mailboxes, _) = index().resolve("acct", &["Label_999".into()]);
        assert_eq!(mailboxes, vec![labels::mailbox_id("acct", "Label_999")]);
    }

    /// Hex (API) and decimal (Takeout `X-GM-THRID`) are the same number;
    /// storing both spellings would split one conversation in two.
    #[test]
    fn normalizes_hex_thread_ids_to_takeouts_decimal() {
        assert_eq!(
            normalize_thread_id("18c9f2a1b2c3d4e5"),
            "1786225503531947237"
        );
        // Pin the relationship, not just the literal: the decimal must be
        // the same 64-bit number the hex spells.
        assert_eq!(
            normalize_thread_id("18c9f2a1b2c3d4e5")
                .parse::<u64>()
                .unwrap(),
            0x18c9f2a1b2c3d4e5,
        );
        assert_eq!(normalize_thread_id("1"), "1");
        // Not hex: leave it alone rather than mangling it.
        assert_eq!(normalize_thread_id("thread-xyz"), "thread-xyz");
    }

    /// Epoch milliseconds carry no source offset, so the convention is an
    /// explicit `+00:00` — never a bare `Z` from a strftime.
    #[test]
    fn renders_internal_date_with_an_explicit_offset() {
        let iso = internal_date_to_iso(1_777_000_925_123);
        assert!(iso.starts_with("2026-"), "{iso}");
        assert!(iso.ends_with("+00:00"), "{iso}");
    }

    #[test]
    fn lists_mailbox_rows_for_every_filing_label() {
        let mailboxes = index().mailboxes("acct");
        let names: Vec<&str> = mailboxes.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"Inbox"));
        assert!(names.contains(&"Category Promotions"));
        assert!(names.contains(&"Work/Projects"));
        // Keyword-only labels are not mailboxes.
        assert!(!names.contains(&"Starred"));
        assert!(!names.contains(&"Unread"));
        let inbox_role = mailboxes.iter().find(|(_, n, _)| n == "Inbox").unwrap().2;
        assert_eq!(inbox_role, Some("inbox"));
    }
}
