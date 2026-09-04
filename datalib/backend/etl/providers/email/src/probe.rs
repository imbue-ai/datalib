//! Read-only account probe: "can these credentials reach this
//! mailbox, and what labels does it have?"
//!
//! This is what the Add-a-source wizard's **Test connection** button
//! runs, and what fills its label pickers. It exists as provider code
//! rather than as something the HTTP server does itself for the same
//! reason download does: the two ways to reach a mailbox (Gmail's REST
//! API, a JMAP server) disagree about almost everything, and the one
//! place that already knows how to reconcile them is here.
//!
//! Three properties are deliberate:
//!
//! * **It writes nothing.** No data root, no doltlite file, no
//!   cursor. A probe is safe to run against a config that has never
//!   synced, and safe to run repeatedly.
//! * **It costs one or two HTTP calls**, never an enumeration. Gmail:
//!   `users.getProfile` + `users.labels.list`. JMAP: session discovery
//!   + one `Mailbox/get`.
//! * **The label strings it returns are exactly the strings
//!   `only_extract_labels` / `only_render_labels` accept.** That is the
//!   whole point — a picker that offered a spelling the filter then
//!   failed to match would be worse than a text box. For Gmail that
//!   means [`labels::canonical_name`] is applied to system labels
//!   (`INBOX` → `Inbox`) and user labels pass through with their full
//!   `Parent/Child` name; for JMAP it means the path walk in
//!   [`crate::mailbox_labels`], which is the same matcher the filters
//!   themselves use.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use datalib_etl_email_config::{EmailConfig, EmailLiveMode};

use crate::download::gmail_api::api as gmail;
use crate::download::labels::{self, LabelMap};
use crate::download::{api, session::Session};
use crate::mailbox_labels::{self, MailboxNode};

/// What a successful probe found. Serialized straight to stdout by
/// `datalib-step probe email` and passed through by the HTTP server, so
/// the field names here are the wire format the UI reads.
#[derive(Debug, Serialize)]
pub struct ProbeReport {
    /// Which download mode was probed — the same word the config uses
    /// to select it (`gmail_api`, `sync`).
    pub mode: &'static str,
    pub account: ProbeAccount,
    /// Every label/mailbox this account has, in display order (roles
    /// first, then alphabetical).
    pub labels: Vec<ProbeLabel>,
    /// Things worth telling the person who clicked the button that
    /// aren't failures.
    pub notes: Vec<String>,
}

/// The account the credentials actually reached. Shown back so "Test
/// connection" answers *which* mailbox as well as *whether* — a
/// latchkey store with two Google accounts in it will happily connect
/// to the wrong one.
#[derive(Debug, Serialize)]
pub struct ProbeAccount {
    /// The provider's own id for it: a JMAP account id, or the Gmail
    /// address `users.getProfile` reported.
    pub id: String,
    /// Canonical email address, when the provider tells us one.
    pub address: Option<String>,
    pub display_name: Option<String>,
    /// Total messages, when the provider reports it cheaply. Gmail
    /// does (`messagesTotal`); JMAP does not without a query.
    pub message_estimate: Option<u64>,
}

/// One entry in a label picker.
#[derive(Debug, Serialize)]
pub struct ProbeLabel {
    /// The exact string to write into `only_extract_labels` /
    /// `only_render_labels`.
    pub path: String,
    /// `mailbox` — a folder emails are filed in, which both the
    /// download filter and the render filter can match.
    ///
    /// `keyword` — Gmail-only. `Starred`, `Important` and `Unread` are
    /// labels on the wire but flags in the schema we store, so they
    /// never become a mailbox row. The download filter still accepts
    /// them (Gmail resolves them server-side), but the render filter
    /// matches mailbox paths and would silently match nothing — so the
    /// UI must not offer them there. That asymmetry is the reason this
    /// field exists.
    pub kind: &'static str,
    /// JMAP role (`inbox`, `sent`, `archive`, …) when the mailbox has
    /// one. Used only for ordering and for a hint in the picker.
    pub role: Option<String>,
    /// Messages filed here, when the provider reports it for free.
    /// JMAP's `Mailbox/get` does; Gmail's `labels.list` does not.
    pub messages: Option<u64>,
}

/// Kinds, spelled once.
const KIND_MAILBOX: &str = "mailbox";
const KIND_KEYWORD: &str = "keyword";

/// Probe whichever live mode `config` selects.
///
/// A config with no live mode is an mbox source, which has no
/// connection to test — the honest answer is an error naming what to
/// do, not an empty report that reads like success.
pub async fn probe(config: &EmailConfig) -> Result<ProbeReport> {
    config.validate()?;
    match config.live_mode()? {
        Some(EmailLiveMode::GmailApi(gmail_cfg)) => {
            probe_gmail(gmail_cfg.user_id(), &config.latchkey_settings).await
        }
        Some(EmailLiveMode::Jmap(sync)) => probe_jmap(sync, &config.latchkey_settings).await,
        None => Err(anyhow!(
            "this email source has no live download mode, so there is no connection to test. \
             Set `gmail_api` for a Gmail account or `sync.hostname` for a JMAP server; an \
             mbox source reads a file at `common.input_path` and needs no credentials."
        )),
    }
}

// ---------------------------------------------------------------------
// Gmail
// ---------------------------------------------------------------------

async fn probe_gmail(
    user_id: &str,
    latchkey: &datalib_etl::http::LatchkeySettings,
) -> Result<ProbeReport> {
    let profile = gmail::get_profile(user_id, latchkey)
        .await
        .context("Gmail users.getProfile")?;
    let raw = gmail::list_labels(user_id, latchkey)
        .await
        .context("Gmail users.labels.list")?;

    let mut labels: Vec<ProbeLabel> = Vec::with_capacity(raw.len());
    for label in &raw {
        // Exactly the mapping `LabelIndex` applies at download time: a
        // system label is canonicalized, a *user* label keeps its own
        // name even when that name collides with a system one.
        let (path, mapping) = if label.is_system {
            (
                labels::canonical_name(&label.name),
                labels::map_label(&label.name),
            )
        } else {
            (label.name.clone(), LabelMap::Mailbox { role: None })
        };
        let (kind, role) = match mapping {
            LabelMap::Mailbox { role } => (KIND_MAILBOX, role.map(str::to_string)),
            LabelMap::Keyword(_) | LabelMap::Unread => (KIND_KEYWORD, None),
            // `Archived` and `Muted` carry nothing we store, so
            // filtering on them could only ever mean "nothing".
            LabelMap::Drop => continue,
        };
        labels.push(ProbeLabel {
            path,
            kind,
            role,
            messages: None,
        });
    }
    dedupe_and_sort(&mut labels);

    Ok(ProbeReport {
        mode: "gmail_api",
        account: ProbeAccount {
            id: profile.email_address.clone(),
            address: Some(profile.email_address),
            display_name: None,
            message_estimate: profile.messages_total,
        },
        labels,
        notes: vec![
            "Gmail reports no per-label message counts without a request per label, so the \
             counts are left blank."
                .to_string(),
        ],
    })
}

// ---------------------------------------------------------------------
// JMAP
// ---------------------------------------------------------------------

async fn probe_jmap(
    sync: &datalib_etl_email_config::EmailSync,
    latchkey: &datalib_etl::http::LatchkeySettings,
) -> Result<ProbeReport> {
    if sync.hostname.trim().is_empty() {
        return Err(anyhow!(
            "this email source selects JMAP but sets no `sync.hostname` \
             (Fastmail's is `api.fastmail.com`)"
        ));
    }
    let session = Session::discover(&sync.hostname, latchkey)
        .await
        .with_context(|| format!("JMAP session discovery against {}", sync.hostname))?;
    let account_id = session.pick_account(sync.account_id.as_deref())?;

    let resp = api::call(
        &session,
        "Mailbox/get",
        json!({
            "accountId": account_id,
            "ids": null,
            "properties": ["id", "name", "parentId", "role", "totalEmails"],
        }),
    )
    .await
    .context("JMAP Mailbox/get")?;
    let list = resp
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Paths come from the same walk the filters use, so what the
    // picker offers is what `resolve` will match.
    let nodes: Vec<MailboxNode> = list.iter().filter_map(MailboxNode::from_payload).collect();
    let paths = mailbox_labels::paths_by_id(&nodes);
    let mut labels: Vec<ProbeLabel> = list
        .iter()
        .filter_map(|mailbox| {
            let id = mailbox.get("id")?.as_str()?;
            Some(ProbeLabel {
                path: paths.get(id)?.clone(),
                kind: KIND_MAILBOX,
                role: mailbox
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                messages: mailbox.get("totalEmails").and_then(Value::as_u64),
            })
        })
        .collect();
    dedupe_and_sort(&mut labels);

    let account = session
        .accounts
        .iter()
        .find(|(id, _)| *id == account_id)
        .map(|(_, v)| v.clone());
    let name = account
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok(ProbeReport {
        mode: "sync",
        account: ProbeAccount {
            id: account_id,
            // JMAP's account `name` is a display name that on Fastmail
            // happens to be the address; report it as both rather than
            // asserting it is one or the other.
            address: name.clone(),
            display_name: name,
            message_estimate: None,
        },
        labels,
        notes: Vec::new(),
    })
}

/// Roles first (Inbox before a user folder), then alphabetical by path.
/// Two mailboxes can share a path — a re-used Gmail label, or sibling
/// folders with one name — and the picker only needs the string once.
fn dedupe_and_sort(labels: &mut Vec<ProbeLabel>) {
    labels.sort_by(|a, b| {
        b.role
            .is_some()
            .cmp(&a.role.is_some())
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut seen = std::collections::HashSet::new();
    labels.retain(|l| seen.insert(l.path.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(path: &str, kind: &'static str, role: Option<&str>) -> ProbeLabel {
        ProbeLabel {
            path: path.to_string(),
            kind,
            role: role.map(str::to_string),
            messages: None,
        }
    }

    #[test]
    fn roles_sort_first_then_alphabetical() {
        let mut labels = vec![
            label("zebra", KIND_MAILBOX, None),
            label("Sent", KIND_MAILBOX, Some("sent")),
            label("apple", KIND_MAILBOX, None),
            label("Inbox", KIND_MAILBOX, Some("inbox")),
        ];
        dedupe_and_sort(&mut labels);
        let paths: Vec<&str> = labels.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["Inbox", "Sent", "apple", "zebra"]);
    }

    /// Two mailboxes can resolve to one path (a re-used Gmail label, or
    /// two sibling folders sharing a name). The picker offers the
    /// string, and the string is the same string.
    #[test]
    fn collapses_duplicate_paths() {
        let mut labels = vec![
            label("Work", KIND_MAILBOX, None),
            label("Work", KIND_MAILBOX, None),
        ];
        dedupe_and_sort(&mut labels);
        assert_eq!(labels.len(), 1);
    }

    /// An mbox source has no credentials and no server, so "test
    /// connection" has to say that rather than report a happy zero-label
    /// mailbox.
    #[test]
    fn refuses_a_source_with_no_live_mode() {
        let cfg = EmailConfig::default();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&cfg))
            .expect_err("mbox has no connection to test")
            .to_string();
        assert!(err.contains("no live download mode"), "{err}");
    }

    /// JMAP with no hostname would otherwise fail deep inside session
    /// discovery with a URL that reads like a bug in datalib.
    #[test]
    fn names_the_missing_jmap_hostname() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "sync": { "hostname": "  " },
        }))
        .unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&cfg))
            .expect_err("an empty hostname cannot be probed")
            .to_string();
        assert!(err.contains("sync.hostname"), "{err}");
    }
}
