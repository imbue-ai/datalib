//! What one Mail account's store should hold, decided from the rows of
//! Mail's Envelope Index alone: which emails, under which mailboxes, with
//! which keywords, in which threads. Pure; [`super::index`] reads the rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{bail, Result};

use super::super::envelope::strip_angle;
use super::super::labels::mailbox_id;

/// `mailboxes`: one per folder or Gmail label, named by URL
/// (`imap://<account>/<path>`, `local://<account>/<path>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxRow {
    pub rowid: i64,
    pub url: String,
}

/// `messages`, with the `Message-ID` from `message_global_data`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageRow {
    pub rowid: i64,
    pub mailbox: i64,
    pub message_id: Option<String>,
    pub conversation_id: i64,
    pub read: bool,
    pub flagged: bool,
    pub deleted: bool,
    /// Mail's flag bits; this reads only `answered`, `draft` and
    /// `forwarded` from them.
    pub flags: i64,
    /// Seconds since 1970.
    pub date_received: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct IndexRows {
    pub mailboxes: Vec<MailboxRow>,
    pub messages: Vec<MessageRow>,
    /// `labels`: (message, mailbox), the other mailboxes a message is in.
    /// Gmail's labels, for a Gmail account.
    pub labels: Vec<(i64, i64)>,
}

const FLAG_ANSWERED: i64 = 1 << 2;
const FLAG_DRAFT: i64 = 1 << 6;
const FLAG_FORWARDED: i64 = 1 << 8;

/// One account Mail holds, as its URL names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Mail's id for it: the URL's host, and its folder's name under `V10`.
    pub uuid: String,
    pub messages: usize,
}

/// A mailbox URL split into the account it is in and its path.
fn split_url(url: &str) -> Option<(&str, String)> {
    let rest = url.split_once("://")?.1;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = urlencoding::decode(path).map_or_else(|_| path.to_string(), |p| p.into_owned());
    Some((host, path))
}

/// Every account the index has a mailbox for, with how many messages it
/// holds, most first.
pub fn accounts(rows: &IndexRows) -> Vec<Account> {
    let account_of: HashMap<i64, &str> = rows
        .mailboxes
        .iter()
        .filter_map(|m| Some((m.rowid, split_url(&m.url)?.0)))
        .collect();
    let mut counts: BTreeMap<&str, usize> = account_of.values().map(|a| (*a, 0)).collect();
    for m in rows.messages.iter().filter(|m| !m.deleted) {
        if let Some(a) = account_of.get(&m.mailbox) {
            *counts.entry(a).or_default() += 1;
        }
    }
    let mut out: Vec<Account> = counts
        .into_iter()
        .map(|(uuid, messages)| Account {
            uuid: uuid.to_string(),
            messages,
        })
        .collect();
    out.sort_by(|a, b| b.messages.cmp(&a.messages).then(a.uuid.cmp(&b.uuid)));
    out
}

/// The accounts a source copies: the one `wanted` names, matched without
/// regard to case, or, with none named, every account holding messages.
pub fn choose_accounts<'a>(
    accounts: &'a [Account],
    wanted: Option<&str>,
) -> Result<Vec<&'a Account>> {
    if let Some(wanted) = wanted {
        return match accounts
            .iter()
            .find(|a| a.uuid.eq_ignore_ascii_case(wanted))
        {
            Some(a) => Ok(vec![a]),
            None => bail!(
                "Apple Mail has no account `{wanted}`; it has {}",
                accounts
                    .iter()
                    .map(|a| format!("{} ({} messages)", a.uuid, a.messages))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    let with_mail: Vec<&Account> = accounts.iter().filter(|a| a.messages > 0).collect();
    if with_mail.is_empty() {
        bail!("Apple Mail's index holds no messages");
    }
    Ok(with_mail)
}

/// An email's id in the store: the account's, then the message's key (its
/// `Message-ID`, or its content hash). The store keys emails across every
/// account, and one message sent to two of them is a copy in each.
pub fn email_id(store_account_id: &str, key: &str) -> String {
    format!("{store_account_id}:{key}")
}

/// What a mailbox is to the person, from its last path component.
pub fn role(path: &str) -> Option<&'static str> {
    if path.eq_ignore_ascii_case("inbox") {
        return Some("inbox");
    }
    let last = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    Some(match last.as_str() {
        "sent" | "sent mail" | "sent messages" | "sent items" => "sent",
        "drafts" => "drafts",
        "trash" | "deleted messages" | "deleted items" | "bin" => "trash",
        "spam" | "junk" | "junk e-mail" | "junk email" => "junk",
        "archive" => "archive",
        "all mail" => "all",
        "important" => "important",
        "starred" | "flagged" => "flagged",
        _ => return None,
    })
}

/// A mailbox's name as a person reads it: its path, with IMAP's `INBOX`
/// spelled the way Mail shows it.
fn display_name(path: &str) -> String {
    if path.eq_ignore_ascii_case("inbox") {
        "Inbox".to_string()
    } else {
        path.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMailbox {
    pub id: String,
    pub name: String,
    pub role: Option<&'static str>,
}

/// One email the store should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEmail {
    /// [`email_id`] of the `Message-ID`; `None` for a message with none,
    /// whose id is made from its content hash once it is read.
    pub email_id: Option<String>,
    /// The index's rows for it: more than one when one message is in the
    /// index twice. The first is the one whose file is read.
    pub rowids: Vec<i64>,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
    /// `None` when the message is a thread of its own and its id is not
    /// known until it is read.
    pub thread_id: Option<String>,
    pub date_received: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub mailboxes: Vec<PlannedMailbox>,
    pub emails: Vec<PlannedEmail>,
    /// Thread → how many emails the account has in it, filter or not.
    pub in_account: HashMap<String, i64>,
    /// Every mailbox name, before the label filter.
    pub names: BTreeSet<String>,
    /// Every email id with a `Message-ID`, before the label filter.
    pub unfiltered_ids: BTreeSet<String>,
}

/// What to mirror of the account `account`: every message not marked
/// deleted, or, under `only_labels`, those in a mailbox named there (and
/// with `whole_threads`, every message of a thread one of those is in).
pub fn plan(
    rows: &IndexRows,
    account: &str,
    store_account_id: &str,
    only_labels: &[String],
    whole_threads: bool,
) -> Plan {
    let mut mailboxes: BTreeMap<i64, PlannedMailbox> = BTreeMap::new();
    for m in &rows.mailboxes {
        let Some((host, path)) = split_url(&m.url) else {
            continue;
        };
        if host != account || path.is_empty() {
            continue;
        }
        let name = display_name(&path);
        mailboxes.insert(
            m.rowid,
            PlannedMailbox {
                id: mailbox_id(store_account_id, &name),
                role: role(&path),
                name,
            },
        );
    }

    let mut other_mailboxes: HashMap<i64, Vec<i64>> = HashMap::new();
    for (message, mailbox) in &rows.labels {
        other_mailboxes.entry(*message).or_default().push(*mailbox);
    }

    // One email per Message-ID: a message the index holds twice is one
    // email in every mailbox either row is in.
    let mut by_id: BTreeMap<String, PlannedEmail> = BTreeMap::new();
    let mut without_id: Vec<PlannedEmail> = Vec::new();
    let mut conversation_of: HashMap<i64, i64> = HashMap::new();
    let mut messages: Vec<&MessageRow> = rows
        .messages
        .iter()
        .filter(|m| !m.deleted && mailboxes.contains_key(&m.mailbox))
        .collect();
    messages.sort_by_key(|m| m.rowid);
    for m in messages {
        conversation_of.insert(m.rowid, m.conversation_id);
        let mut mailbox_ids: Vec<String> = std::iter::once(m.mailbox)
            .chain(other_mailboxes.get(&m.rowid).into_iter().flatten().copied())
            .filter_map(|rowid| mailboxes.get(&rowid).map(|mb| mb.id.clone()))
            .collect();
        let mut keywords = Vec::new();
        for (on, keyword) in [
            (m.read, "$seen"),
            (m.flagged, "$flagged"),
            (m.flags & FLAG_ANSWERED != 0, "$answered"),
            (m.flags & FLAG_DRAFT != 0, "$draft"),
            (m.flags & FLAG_FORWARDED != 0, "$forwarded"),
        ] {
            if on {
                keywords.push(keyword.to_string());
            }
        }
        let email_id = m
            .message_id
            .as_deref()
            .map(|id| strip_angle(id.trim()))
            .filter(|id| !id.is_empty())
            .map(|id| email_id(store_account_id, id));
        match email_id {
            Some(id) => {
                let entry = by_id.entry(id.clone()).or_insert_with(|| PlannedEmail {
                    email_id: Some(id),
                    rowids: Vec::new(),
                    mailbox_ids: Vec::new(),
                    keywords: Vec::new(),
                    thread_id: None,
                    date_received: m.date_received,
                });
                entry.rowids.push(m.rowid);
                entry.mailbox_ids.append(&mut mailbox_ids);
                entry.keywords.append(&mut keywords);
            }
            None => without_id.push(PlannedEmail {
                email_id: None,
                rowids: vec![m.rowid],
                mailbox_ids,
                keywords,
                thread_id: None,
                date_received: m.date_received,
            }),
        }
    }
    let mut emails: Vec<PlannedEmail> = by_id.into_values().chain(without_id).collect();
    for e in &mut emails {
        e.mailbox_ids.sort();
        e.mailbox_ids.dedup();
        e.keywords.sort();
        e.keywords.dedup();
    }

    // A conversation is keyed by its first message that has a Message-ID,
    // which, unlike Mail's own number for it, outlives a rebuilt index.
    let mut first_of_conversation: HashMap<i64, (Option<i64>, String)> = HashMap::new();
    for e in &emails {
        let Some(id) = &e.email_id else { continue };
        let conversation = conversation_of[&e.rowids[0]];
        if conversation == 0 {
            continue;
        }
        let key = (e.date_received, id.clone());
        first_of_conversation
            .entry(conversation)
            .and_modify(|held| {
                if (key.0, &key.1) < (held.0, &held.1) {
                    *held = key.clone();
                }
            })
            .or_insert(key);
    }
    for e in &mut emails {
        let conversation = conversation_of[&e.rowids[0]];
        e.thread_id = first_of_conversation
            .get(&conversation)
            .map(|(_, id)| id.clone())
            .or_else(|| e.email_id.clone());
    }

    let mut in_account: HashMap<String, i64> = HashMap::new();
    for e in &emails {
        if let Some(t) = &e.thread_id {
            *in_account.entry(t.clone()).or_default() += 1;
        }
    }
    let names: BTreeSet<String> = mailboxes.values().map(|m| m.name.clone()).collect();
    let unfiltered_ids: BTreeSet<String> =
        emails.iter().filter_map(|e| e.email_id.clone()).collect();

    if !only_labels.is_empty() {
        let wanted: HashSet<&str> = mailboxes
            .values()
            .filter(|m| only_labels.iter().any(|l| l.trim() == m.name))
            .map(|m| m.id.as_str())
            .collect();
        let matches = |e: &PlannedEmail| e.mailbox_ids.iter().any(|m| wanted.contains(m.as_str()));
        let kept_threads: HashSet<String> = if whole_threads {
            emails
                .iter()
                .filter(|e| matches(e))
                .filter_map(|e| e.thread_id.clone())
                .collect()
        } else {
            HashSet::new()
        };
        emails.retain(|e| {
            matches(e)
                || e.thread_id
                    .as_ref()
                    .is_some_and(|t| kept_threads.contains(t))
        });
    }

    Plan {
        mailboxes: mailboxes.into_values().collect(),
        emails,
        in_account,
        names,
        unfiltered_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "1F0B8A52-0000-4000-8000-00000000E1D0";

    fn mailbox(rowid: i64, path: &str) -> MailboxRow {
        MailboxRow {
            rowid,
            url: format!("imap://{ACCOUNT}/{path}"),
        }
    }

    fn message(rowid: i64, mailbox: i64, id: &str, conversation: i64, date: i64) -> MessageRow {
        MessageRow {
            rowid,
            mailbox,
            message_id: Some(format!("<{id}>")),
            conversation_id: conversation,
            date_received: Some(date),
            ..MessageRow::default()
        }
    }

    fn rows() -> IndexRows {
        IndexRows {
            mailboxes: vec![
                mailbox(1, "INBOX"),
                mailbox(2, "%5BGmail%5D/All%20Mail"),
                mailbox(3, "Away%20Team/Reports"),
                MailboxRow {
                    rowid: 4,
                    url: "local://2C7E0D11-0000-4000-8000-0000000000AA/Outbox".into(),
                },
            ],
            messages: vec![
                message(10, 2, "first@enterprise.example", 7, 100),
                message(11, 2, "reply@enterprise.example", 7, 200),
                message(12, 2, "lone@enterprise.example", 8, 300),
                MessageRow {
                    read: true,
                    flags: FLAG_ANSWERED,
                    ..message(13, 2, "dup@enterprise.example", 9, 400)
                },
                MessageRow {
                    flagged: true,
                    ..message(14, 1, "dup@enterprise.example", 9, 400)
                },
                MessageRow {
                    deleted: true,
                    ..message(15, 2, "gone@enterprise.example", 10, 500)
                },
                message(16, 4, "outbox@enterprise.example", 11, 600),
            ],
            labels: vec![(11, 3), (12, 1)],
        }
    }

    #[test]
    fn every_account_with_mail_is_copied_unless_one_is_named() {
        let accounts = accounts(&rows());
        assert_eq!(accounts[0].uuid, ACCOUNT);
        assert_eq!(accounts[0].messages, 5);
        assert_eq!(accounts[1].messages, 1);
        let uuids = |chosen: Vec<&Account>| -> Vec<String> {
            chosen.into_iter().map(|a| a.uuid.clone()).collect()
        };
        assert_eq!(
            uuids(choose_accounts(&accounts, None).unwrap()),
            [ACCOUNT, "2C7E0D11-0000-4000-8000-0000000000AA"]
        );
        let lower = ACCOUNT.to_ascii_lowercase();
        assert_eq!(
            uuids(choose_accounts(&accounts, Some(&lower)).unwrap()),
            [ACCOUNT]
        );
        let err = choose_accounts(&accounts, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no account `nope`"), "{err}");
    }

    /// The same message in two accounts is two emails, one in each.
    #[test]
    fn a_message_in_two_accounts_is_an_email_in_each() {
        let mut rows = rows();
        rows.messages
            .push(message(17, 4, "first@enterprise.example", 7, 100));
        let ids = |account: &str, store: &str| -> Vec<String> {
            plan(&rows, account, store, &[], true)
                .emails
                .into_iter()
                .filter_map(|e| e.email_id)
                .filter(|id| id.ends_with(":first@enterprise.example"))
                .collect()
        };
        assert_eq!(ids(ACCOUNT, "acct"), ["acct:first@enterprise.example"]);
        assert_eq!(
            ids("2C7E0D11-0000-4000-8000-0000000000AA", "local"),
            ["local:first@enterprise.example"]
        );
    }

    #[test]
    fn mailboxes_are_named_by_their_decoded_path_with_a_role() {
        let plan = plan(&rows(), ACCOUNT, "acct", &[], true);
        let names: Vec<(&str, Option<&str>)> = plan
            .mailboxes
            .iter()
            .map(|m| (m.name.as_str(), m.role))
            .collect();
        assert_eq!(
            names,
            [
                ("Inbox", Some("inbox")),
                ("[Gmail]/All Mail", Some("all")),
                ("Away Team/Reports", None)
            ]
        );
    }

    /// A message in the index twice is one email, in both mailboxes, with
    /// the keywords of both rows.
    #[test]
    fn a_message_in_the_index_twice_is_one_email() {
        let plan = plan(&rows(), ACCOUNT, "acct", &[], true);
        let dup = plan
            .emails
            .iter()
            .find(|e| e.email_id.as_deref() == Some("acct:dup@enterprise.example"))
            .unwrap();
        assert_eq!(dup.rowids, [13, 14]);
        assert_eq!(dup.mailbox_ids.len(), 2);
        assert_eq!(dup.keywords, ["$answered", "$flagged", "$seen"]);
        assert!(plan
            .emails
            .iter()
            .all(|e| e.email_id.as_deref() != Some("acct:gone@enterprise.example")));
        assert!(plan
            .emails
            .iter()
            .all(|e| e.email_id.as_deref() != Some("acct:outbox@enterprise.example")));
    }

    #[test]
    fn a_thread_is_keyed_by_its_first_message() {
        let plan = plan(&rows(), ACCOUNT, "acct", &[], true);
        let thread = |id: &str| {
            plan.emails
                .iter()
                .find(|e| e.email_id.as_deref() == Some(id))
                .unwrap()
                .thread_id
                .clone()
                .unwrap()
        };
        assert_eq!(
            thread("acct:reply@enterprise.example"),
            "acct:first@enterprise.example"
        );
        assert_eq!(
            thread("acct:first@enterprise.example"),
            "acct:first@enterprise.example"
        );
        assert_eq!(
            thread("acct:lone@enterprise.example"),
            "acct:lone@enterprise.example"
        );
        assert_eq!(plan.in_account["acct:first@enterprise.example"], 2);
    }

    #[test]
    fn a_label_filter_keeps_the_rest_of_a_thread_only_when_asked() {
        let filter = vec!["Away Team/Reports".to_string()];
        let ids =
            |p: Plan| -> Vec<String> { p.emails.into_iter().filter_map(|e| e.email_id).collect() };
        assert_eq!(
            ids(plan(&rows(), ACCOUNT, "acct", &filter, true)),
            [
                "acct:first@enterprise.example",
                "acct:reply@enterprise.example"
            ]
        );
        assert_eq!(
            ids(plan(&rows(), ACCOUNT, "acct", &filter, false)),
            ["acct:reply@enterprise.example"]
        );
    }
}
