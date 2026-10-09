//! The people a chat document shows, as chat-common alone can describe
//! them: each author handle, the names the provider showed it under, and
//! how much it wrote; whom a message went to and who reacted, having
//! written nothing. Every chat provider gets this with no code of its
//! own; a provider that knows more says so in its own `NormalizedContact`s.

use std::collections::BTreeMap;

use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact, Seen};
use datalib_time::IsoOffsetTimestamp;

use crate::types::{NormalizedChatItem, OrphanReactions};

pub fn baseline_contacts(
    source_id: &str,
    items: &[NormalizedChatItem],
    orphans: &[OrphanReactions],
) -> Vec<NormalizedContact> {
    struct Tally {
        names: Vec<(String, u64)>,
        items: u64,
        last_ms: Option<i64>,
    }
    let mut by_handle: BTreeMap<&str, (&datalib_handle::Handle, Tally)> = BTreeMap::new();
    // Whom an item was addressed to, and who reacted to it or to a
    // message not in the mirror: in the document, but writing nothing.
    let recipients = items
        .iter()
        .flat_map(|i| &i.recipients)
        .filter_map(|r| Some((r.handle.as_ref()?, &r.display)));
    let reactors = items
        .iter()
        .flat_map(|i| &i.reactions)
        .chain(orphans.iter().flat_map(|o| &o.reactions))
        .filter_map(|r| Some((r.reactor_handle.as_ref()?, &r.reactor_display)));
    let named_only = recipients.chain(reactors);
    for (handle, display) in named_only {
        let (_, tally) = by_handle.entry(handle.as_str()).or_insert((
            handle,
            Tally {
                names: Vec::new(),
                items: 0,
                last_ms: None,
            },
        ));
        let name = name_without_handle(display, handle);
        if !name.is_empty() && !name.eq_ignore_ascii_case(handle.value()) {
            match tally.names.iter_mut().find(|(n, _)| n == name) {
                Some((_, count)) => *count += 1,
                None => tally.names.push((name.to_string(), 1)),
            }
        }
    }
    for item in items {
        let Some(handle) = &item.author_handle else {
            continue;
        };
        let (_, tally) = by_handle.entry(handle.as_str()).or_insert((
            handle,
            Tally {
                names: Vec::new(),
                items: 0,
                last_ms: None,
            },
        ));
        tally.items += 1;
        tally.last_ms = tally.last_ms.max(item.date_ms);
        let name = name_without_handle(&item.author_display, handle);
        if !name.is_empty() {
            match tally.names.iter_mut().find(|(n, _)| n == name) {
                Some((_, count)) => *count += 1,
                None => tally.names.push((name.to_string(), 1)),
            }
        }
    }
    by_handle
        .into_values()
        .map(|(handle, mut tally)| {
            // Most used first; a tie keeps the order they appeared in.
            tally
                .names
                .sort_by_key(|(_, count)| std::cmp::Reverse(*count));
            let mut c = NormalizedContact::new(source_id, handle.as_str(), ContactKind::Person);
            c.names = tally.names.into_iter().map(|(n, _)| n).collect();
            c.handles = vec![ContactHandle::of(handle.clone())];
            c.seen = Some(Seen {
                items: tally.items,
                last_at: tally
                    .last_ms
                    .and_then(IsoOffsetTimestamp::from_unix_millis)
                    .map(|t| t.to_rfc3339_secs()),
            });
            c
        })
        .collect()
}

/// The people a document carries: chat-common's baseline for each author
/// handle, and where the provider gave its own source contact for the person
/// behind one, that instead — keeping what the baseline counted
/// and every name it saw. One provider source contact reached through two
/// handles is one person.
pub fn document_contacts(
    source_id: &str,
    items: &[NormalizedChatItem],
    orphans: &[OrphanReactions],
    provider: &[NormalizedContact],
) -> Vec<NormalizedContact> {
    let mut out: BTreeMap<String, NormalizedContact> = BTreeMap::new();
    for seen in baseline_contacts(source_id, items, orphans) {
        let theirs = provider.iter().find(|p| {
            p.handles
                .iter()
                .any(|h| h.handle.as_ref().map(|h| h.as_str()) == Some(seen.key.as_str()))
        });
        let Some(theirs) = theirs else {
            out.insert(seen.key.clone(), seen);
            continue;
        };
        let merged = out.entry(theirs.key.clone()).or_insert_with(|| {
            let mut c = theirs.clone();
            c.source_id = source_id.to_string();
            c.seen = None;
            c
        });
        for name in seen.names {
            if !merged.names.contains(&name) {
                merged.names.push(name);
            }
        }
        merged.seen = match (merged.seen.take(), seen.seen) {
            (Some(a), Some(b)) => Some(Seen {
                items: a.items + b.items,
                last_at: a.last_at.max(b.last_at),
            }),
            (a, b) => a.or(b),
        };
    }
    out.into_values().collect()
}

/// What a header shows as `Will Riker <riker@enterprise.org>` names the
/// person `Will Riker`: the handle's own value, in angle brackets, is the
/// handle, not part of the name.
fn name_without_handle<'a>(shown: &'a str, handle: &datalib_handle::Handle) -> &'a str {
    let shown = shown.trim();
    let suffix = format!("<{}>", handle.value());
    let name = match shown.len().checked_sub(suffix.len()) {
        Some(at) if shown.is_char_boundary(at) && shown[at..].eq_ignore_ascii_case(&suffix) => {
            shown[..at].trim()
        }
        _ => shown,
    };
    name.strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemKind;
    use datalib_handle::Handle;

    fn by(handle: Option<&Handle>, name: &str, ms: i64) -> NormalizedChatItem {
        NormalizedChatItem {
            message_uuid: format!("m{ms}"),
            author_handle: handle.cloned(),
            author_display: name.to_string(),
            date_ms: Some(ms),
            text: Some("hi".to_string()),
            kind: ItemKind::Text,
            attachments: vec![],
            reactions: vec![],
            labels: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: None,
            source_ref: None,
            is_aside: false,
            branch: Vec::new(),
            unread: false,
            recipients: Vec::new(),
            mentions: Vec::new(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn one_per_handle_with_its_names_count_and_last_stamp() {
        let riker = Handle::email("riker@enterprise.org").unwrap();
        let troi = Handle::tel("+15550101010").unwrap();
        let items = vec![
            by(Some(&riker), "Will Riker", 1_000),
            by(Some(&troi), "Deanna", 2_000),
            by(Some(&riker), "Number One", 3_000),
            by(Some(&riker), "Number One", 4_000),
            by(None, "Me", 5_000),
        ];
        let got = baseline_contacts("mail", &items, &[]);
        assert_eq!(got.len(), 2, "the owner's own items have no handle");
        let r = &got[0];
        assert_eq!(r.key, "email:riker@enterprise.org");
        assert_eq!(r.source_id, "mail");
        assert_eq!(r.names, ["Number One", "Will Riker"], "most used first");
        assert_eq!(r.handles[0].handle.as_ref(), Some(&riker));
        assert_eq!(r.seen.as_ref().unwrap().items, 3);
        assert_eq!(
            r.seen.as_ref().unwrap().last_at.as_deref(),
            Some("1970-01-01T00:00:04+00:00")
        );
        assert_eq!(got[1].names, ["Deanna"]);
    }

    #[test]
    fn a_name_shown_with_its_address_is_the_name() {
        let riker = Handle::email("riker@enterprise.org").unwrap();
        let items = vec![
            by(Some(&riker), "Will Riker <Riker@Enterprise.org>", 1),
            by(Some(&riker), "\"Riker, Will\" <riker@enterprise.org>", 2),
            by(Some(&riker), "<riker@enterprise.org>", 3),
        ];
        let got = baseline_contacts("mail", &items, &[]);
        assert_eq!(got[0].names, ["Will Riker", "Riker, Will"]);
    }

    /// A Slack user's profile knows their email; the messages only their
    /// user id. The document carries the profile, counted the way the
    /// baseline counted, so the email reaches the index too.
    #[test]
    fn a_providers_account_replaces_the_baseline_and_keeps_its_count() {
        let picard = Handle::slack("T1", "U_PICARD").unwrap();
        let riker = Handle::slack("T1", "U_RIKER").unwrap();
        let mut profile =
            NormalizedContact::new("ignored", "slack:T1/U_PICARD", ContactKind::Person);
        profile.names = vec!["Jean-Luc Picard".into()];
        profile.title = Some("Captain".into());
        profile.handles = vec![
            ContactHandle::of(picard.clone()),
            ContactHandle::email(None, "picard@enterprise.org"),
        ];
        let items = vec![
            by(Some(&picard), "Picard", 1),
            by(Some(&picard), "Picard", 2),
            by(Some(&riker), "Riker", 3),
        ];
        let got = document_contacts("slack", &items, &[], &[profile]);
        assert_eq!(got.len(), 2);
        let p = got.iter().find(|c| c.key == "slack:T1/U_PICARD").unwrap();
        assert_eq!(p.source_id, "slack");
        assert_eq!(p.title.as_deref(), Some("Captain"));
        assert_eq!(p.names, ["Jean-Luc Picard", "Picard"]);
        assert_eq!(p.handles.len(), 2, "the email rides along");
        assert_eq!(p.seen.as_ref().unwrap().items, 2);
        let r = got.iter().find(|c| c.key == "slack:T1/U_RIKER").unwrap();
        assert_eq!(r.names, ["Riker"], "no profile: the baseline as it was");
    }

    /// Someone who only reacted is in the document too, under the name
    /// the provider showed for them, having written nothing.
    #[test]
    fn reactors_are_in_the_document_with_nothing_written() {
        use crate::types::NormalizedReaction;
        let picard = Handle::email("picard@enterprise.org").unwrap();
        let worf = Handle::tel("+15550104040").unwrap();
        let mut item = by(Some(&picard), "Jean-Luc Picard", 1);
        let react = |handle: Option<&Handle>, who: &str| NormalizedReaction {
            reaction_uuid: format!("r-{who}"),
            reactor_handle: handle.cloned(),
            reactor_display: who.to_string(),
            emoji: "🫡".to_string(),
            date_ms: Some(2),
            source_ref: None,
        };
        item.reactions = vec![
            react(Some(&worf), "Worf"),
            react(Some(&picard), "Jean-Luc Picard"),
            react(None, "Me"),
        ];
        let got = baseline_contacts("chat", &[item], &[]);
        assert_eq!(got.len(), 2, "a reaction with no handle names nobody");
        let w = got.iter().find(|c| c.key == worf.as_str()).unwrap();
        assert_eq!(w.names, ["Worf"]);
        assert_eq!(w.seen.as_ref().unwrap().items, 0);
        let p = got.iter().find(|c| c.key == picard.as_str()).unwrap();
        assert_eq!(
            p.seen.as_ref().unwrap().items,
            1,
            "a reaction is not an item written"
        );
    }

    /// A reaction to a message not in the mirror names its reactor too:
    /// the orphan list draws them as chips, so the index has to know them.
    #[test]
    fn a_reactor_to_a_missing_message_is_in_the_document() {
        use crate::types::{NormalizedReaction, OrphanReactions};
        let worf = Handle::tel("+15550104040").unwrap();
        let orphans = vec![OrphanReactions {
            target_native_id: "gone".to_string(),
            reactions: vec![NormalizedReaction {
                reaction_uuid: "r-worf".to_string(),
                reactor_handle: Some(worf.clone()),
                reactor_display: "Worf".to_string(),
                emoji: "🫡".to_string(),
                date_ms: Some(2),
                source_ref: None,
            }],
        }];
        let got = baseline_contacts("chat", &[], &orphans);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].key, worf.as_str());
        assert_eq!(got[0].names, ["Worf"]);
        assert_eq!(got[0].seen.as_ref().unwrap().items, 0);
    }

    /// Whom a message went to is in the document too, having written
    /// nothing there: a chip on a To line needs to find them.
    #[test]
    fn recipients_are_in_the_document_with_nothing_written() {
        use crate::types::{Recipient, RecipientRole};
        let picard = Handle::email("picard@enterprise.org").unwrap();
        let troi = Handle::email("troi@enterprise.org").unwrap();
        let mut item = by(Some(&picard), "Jean-Luc Picard", 1);
        item.recipients = vec![
            Recipient {
                role: RecipientRole::To,
                display: "Deanna Troi".into(),
                handle: Some(troi.clone()),
            },
            Recipient {
                role: RecipientRole::Cc,
                display: "picard@enterprise.org".into(),
                handle: Some(picard.clone()),
            },
        ];
        let got = baseline_contacts("mail", &[item], &[]);
        let t = got.iter().find(|c| c.key == troi.as_str()).unwrap();
        assert_eq!(t.names, ["Deanna Troi"]);
        assert_eq!(t.seen.as_ref().unwrap().items, 0);
        let p = got.iter().find(|c| c.key == picard.as_str()).unwrap();
        assert_eq!(p.names, ["Jean-Luc Picard"], "a bare address is no name");
        assert_eq!(p.seen.as_ref().unwrap().items, 1);
    }
}
