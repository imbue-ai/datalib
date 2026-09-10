//! Read-only account probe: "can these credentials reach this
//! mailbox, and what labels does it have?" The report's shape is
//! shared with every other probeable provider — see
//! `datalib_source_common::probe`.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use datalib_etl_email_config::{EmailConfig, EmailLiveMode};
use datalib_source_common::probe::{ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport};

use crate::ingest::gmail_api::api as gmail;
use crate::ingest::labels::{self, LabelMap};
use crate::ingest::{api, session::Session};
use crate::mailbox_labels::{self, MailboxNode};

pub async fn probe(config: &EmailConfig) -> Result<ProbeReport> {
    config.validate()?;
    match config.live_mode()? {
        Some(EmailLiveMode::GmailApi(gmail_cfg)) => {
            probe_gmail(gmail_cfg.user_id(), &config.latchkey_settings).await
        }
        Some(EmailLiveMode::Jmap(sync)) => probe_jmap(sync, &config.latchkey_settings).await,
        None => Err(anyhow!(
            "this email source has no live download mode, so there is no connection to test. \
             Set `gmail` for a Gmail account or `jmap.hostname` for a JMAP server; an \
             mbox source reads a file at `mbox.path` and needs no credentials."
        )),
    }
}

// Gmail

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

    let mut items: Vec<ProbeItem> = Vec::with_capacity(raw.len());
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
            LabelMap::Mailbox { role } => (ProbeItemKind::Mailbox, role.map(str::to_string)),
            LabelMap::Keyword(_) | LabelMap::Unread => (ProbeItemKind::Keyword, None),
            // `Archived` and `Muted` carry nothing we store, so
            // filtering on them could only ever mean "nothing".
            LabelMap::Drop => continue,
        };
        items.push(ProbeItem {
            role,
            ..ProbeItem::new(path, kind)
        });
    }
    dedupe_and_sort(&mut items);

    Ok(ProbeReport {
        mode: "gmail".to_string(),
        account: ProbeAccount {
            id: profile.email_address.clone(),
            address: Some(profile.email_address),
            display_name: None,
            message_estimate: profile.messages_total,
        },
        items,
        notes: vec![
            "Gmail reports no per-label message counts without a request per label, so the \
             counts are left blank."
                .to_string(),
        ],
    })
}

// JMAP

async fn probe_jmap(
    sync: &datalib_etl_email_config::EmailSync,
    latchkey: &datalib_etl::http::LatchkeySettings,
) -> Result<ProbeReport> {
    if sync.hostname.trim().is_empty() {
        return Err(anyhow!(
            "this email source selects JMAP but sets no `jmap.hostname` \
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
    let mut items: Vec<ProbeItem> = list
        .iter()
        .filter_map(|mailbox| {
            let id = mailbox.get("id")?.as_str()?;
            Some(ProbeItem {
                role: mailbox
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                messages: mailbox.get("totalEmails").and_then(Value::as_u64),
                ..ProbeItem::new(paths.get(id)?.clone(), ProbeItemKind::Mailbox)
            })
        })
        .collect();
    dedupe_and_sort(&mut items);

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
        mode: "jmap".to_string(),
        account: ProbeAccount {
            id: account_id,
            // JMAP's account `name` is a display name that on Fastmail
            // happens to be the address; report it as both rather than
            // asserting it is one or the other.
            address: name.clone(),
            display_name: name,
            message_estimate: None,
        },
        items,
        notes: Vec::new(),
    })
}

fn dedupe_and_sort(items: &mut Vec<ProbeItem>) {
    items.sort_by(|a, b| {
        b.role
            .is_some()
            .cmp(&a.role.is_some())
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.path.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(path: &str, kind: ProbeItemKind, role: Option<&str>) -> ProbeItem {
        ProbeItem {
            role: role.map(str::to_string),
            ..ProbeItem::new(path, kind)
        }
    }

    #[test]
    fn roles_sort_first_then_alphabetical() {
        let mut items = vec![
            label("zebra", ProbeItemKind::Mailbox, None),
            label("Sent", ProbeItemKind::Mailbox, Some("sent")),
            label("apple", ProbeItemKind::Mailbox, None),
            label("Inbox", ProbeItemKind::Mailbox, Some("inbox")),
        ];
        dedupe_and_sort(&mut items);
        let paths: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["Inbox", "Sent", "apple", "zebra"]);
    }

    /// Two mailboxes can resolve to one path (a re-used Gmail label, or
    /// two sibling folders sharing a name). The picker offers the
    /// string, and the string is the same string.
    #[test]
    fn collapses_duplicate_paths() {
        let mut items = vec![
            label("Work", ProbeItemKind::Mailbox, None),
            label("Work", ProbeItemKind::Mailbox, None),
        ];
        dedupe_and_sort(&mut items);
        assert_eq!(items.len(), 1);
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
            "jmap": { "hostname": "  " },
        }))
        .unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&cfg))
            .expect_err("an empty hostname cannot be probed")
            .to_string();
        assert!(err.contains("jmap.hostname"), "{err}");
    }
}
