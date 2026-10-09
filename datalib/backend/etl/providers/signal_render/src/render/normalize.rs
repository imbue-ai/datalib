//! Signal's parsed buckets, in the shape `chat-common` renders.
//!
//! Signal used to write its own markdown — a bulleted list with a raw
//! ISO stamp per line — and its own grid rows. Everything below turns
//! its parse output into the normalized types instead, so one renderer
//! serves it and the seven other chat sources alike.

use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
use datalib_handle::Handle;
use std::collections::HashMap;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_render::inputs::{Inputs, Lookup};

use super::ids;
use super::parse::{ParsedChat, ParsedChatItem, ParsedRecipient, ParsedSignal};

/// One `NormalizedChat` per *bucket*, not per chat.
///
/// `chat-common` keys attachment bundles by `NormalizedChat::id`, and
/// Signal's parse loads a bundle per bucket rather than per chat — so a
/// chat spanning three periods carries three bundles with nothing to
/// merge them. Splitting per bucket keys them apart without copying
/// bytes around. Nothing downstream notices: the rendered path is
/// `<chat_uuid>/<period>.md` either way, and the chat-level grid row
/// was already one per document.
pub fn to_chats(
    parsed: &ParsedSignal,
    source_id: &str,
) -> (Vec<NormalizedChat>, HashMap<String, BlobBundle>) {
    let mut chats = Vec::with_capacity(parsed.docs.len());
    let mut blobs_by_chat = HashMap::new();

    for doc in &parsed.docs {
        let Some(chat) = parsed.chats.get(&doc.chat_id) else {
            tracing::warn!(
                event = "signal_render_missing_chat",
                chat_id = %doc.chat_id,
                period_key = %doc.period_key,
                "bucket names a chat the parse did not produce; skipping"
            );
            continue;
        };
        let chat_id = ids::chat(source_id, &chat.id);
        let bundle_key = format!("{}#{}", chat.id, doc.period_key);
        // Recipients are declared as they are looked up, on the chat's
        // inputs — every period of the chat shares one declaration.
        let no_inputs = Inputs::default();
        let inputs = parsed.inputs.get(&chat.id).unwrap_or(&no_inputs);
        let recipients = inputs.lookup("recipients", &parsed.recipients);
        let by_aci: HashMap<&str, &str> = parsed
            .recipients
            .values()
            .filter_map(|r| Some((r.aci.as_deref()?, r.id.as_str())))
            .collect();

        let items: Vec<NormalizedChatItem> = doc
            .items
            .iter()
            // An item with neither text nor an attachment is a
            // ChatUpdate or a sticker: nothing a transcript can show.
            // The old renderer skipped these too, but kept counting
            // them, so `message_index` disagreed with the rendered
            // order whenever one appeared.
            .filter(|i| i.text.is_some() || !i.attachments.is_empty())
            .map(|item| to_item(recipients, &by_aci, chat, item, source_id))
            .collect();

        chats.push(NormalizedChat {
            path_prefix: None,
            id: bundle_key.clone(),
            chat_uuid: chat_id.uuid,
            display: recipient_display(recipients, chat),
            // `None`, so chat-common derives the familiar
            // "Signal · {recipient}" heading rather than us restating it.
            title: None,
            author: None,
            account: None,
            project: None,
            external_id: Some(chat_id.natural_key),
            upstream_account: None,
            // Signal Android backups expose no per-thread web URL.
            source_url: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![{
                let period = ids::period(source_id, &chat.id, &doc.period_key);
                NormalizedDoc {
                    orphan_reactions: Vec::new(),
                    period_key: doc.period_key.clone(),
                    markdown_uuid: period.uuid,
                    source_ref: Some(UpstreamRef::new(period.entity_kind, period.natural_key)),
                    items,
                }
            }],
            contacts: contacts_of(recipients, &doc.items, source_id),
            inputs: inputs.declared(),
        });
        if !doc.blobs.is_empty() {
            blobs_by_chat.insert(bundle_key, doc.blobs.clone());
        }
    }
    (chats, blobs_by_chat)
}

fn to_item(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    by_aci: &HashMap<&str, &str>,
    chat: &ParsedChat,
    item: &ParsedChatItem,
    source_id: &str,
) -> NormalizedChatItem {
    let attachments: Vec<NormalizedAttachment> = item
        .attachments
        .iter()
        .map(|a| NormalizedAttachment {
            // chat-common fills this in once it has materialized the
            // bytes for the `ref_id` below.
            rel_path: None,
            file_name: a.file_name.clone(),
            // The bundle's own `is_image` flag has no place in the
            // normalized shape, which reads image-ness off the MIME
            // type; keep the flag's answer when upstream gave no type.
            mime_type: a
                .content_type
                .clone()
                .or_else(|| a.is_image.then(|| "image/*".to_string())),
            byte_len: None,
            source_url: None,
            ref_id: Some(a.ref_id.clone()),
        })
        .collect();

    let id = ids::message(source_id, &chat.id, &item.author_id, item.date_sent);
    let (text, mentions) = match &item.text {
        Some(text) => {
            let (text, mentions) = with_mentions(recipients, by_aci, text, &item.mentions);
            (Some(text), mentions)
        }
        None => (None, Vec::new()),
    };
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_handle: author_handle(recipients, item),
        author_display: author_display(recipients, item),
        date_ms: Some(item.date_sent),
        text,
        kind: if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        },
        attachments,
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread: item.unread,
        recipients: Vec::new(),
        mentions,
        problems: Vec::new(),
    }
}

/// `text` with each mention's `U+FFFC` placeholder written as `@Name`,
/// and the people mentioned, each once: by number where Signal has it,
/// as an author is, else by ACI. A placeholder with no readable ACI
/// behind it is left as it was.
fn with_mentions(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    by_aci: &HashMap<&str, &str>,
    text: &str,
    acis: &[Option<String>],
) -> (String, Vec<Handle>) {
    let mut acis = acis.iter();
    let mut out = String::with_capacity(text.len());
    let mut mentioned: Vec<Handle> = Vec::new();
    for c in text.chars() {
        let aci = match c {
            '\u{FFFC}' => acis.next().and_then(Option::as_deref),
            _ => None,
        };
        let Some(aci) = aci else {
            out.push(c);
            continue;
        };
        let recipient = by_aci.get(aci).and_then(|id| recipients.get(id));
        out.push('@');
        out.push_str(&recipient.map_or_else(|| aci.to_string(), |r| r.display()));
        let handle = recipient
            .and_then(|r| handles_of(r).into_iter().next())
            .or_else(|| Handle::signal_aci(aci));
        if let Some(handle) = handle.filter(|h| !mentioned.contains(h)) {
            mentioned.push(handle);
        }
    }
    (out, mentioned)
}

fn recipient_display(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    chat: &ParsedChat,
) -> String {
    recipients
        .get(&chat.recipient_id)
        .map(|r| r.display())
        .unwrap_or_else(|| format!("recipient_{}", chat.recipient_id))
}

/// A recipient's handles: their number, where the backup has it, and
/// their ACI. The number first, so one link covers every app that
/// reaches them by it; a recipient known by PNI alone has none.
fn handles_of(r: &ParsedRecipient) -> Vec<Handle> {
    [
        r.identifier.as_deref().and_then(Handle::tel),
        r.aci.as_deref().and_then(Handle::signal_aci),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn author_handle(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    item: &ParsedChatItem,
) -> Option<Handle> {
    if item.outgoing {
        return None;
    }
    recipients
        .get(&item.author_id)
        .and_then(|r| handles_of(r).into_iter().next())
}

/// Signal's own account of each person who wrote in the bucket: the
/// name the backup shows and every handle it has for them. This is
/// where a number and an ACI are tied together, so a link made through
/// either finds the other.
fn contacts_of(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    items: &[ParsedChatItem],
    source_id: &str,
) -> Vec<NormalizedContact> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|i| !i.outgoing)
        .filter(|i| seen.insert(i.author_id.clone()))
        .filter_map(|i| recipients.get(&i.author_id))
        .filter_map(|r| {
            let handles = handles_of(r);
            let first = handles.first()?;
            let mut c = NormalizedContact::new(source_id, first.as_str(), ContactKind::Person);
            c.names = r.display_name.iter().cloned().collect();
            c.handles = handles.into_iter().map(ContactHandle::of).collect();
            Some(c)
        })
        .collect()
}

fn author_display(
    recipients: Lookup<'_, HashMap<String, ParsedRecipient>>,
    item: &ParsedChatItem,
) -> String {
    if item.outgoing {
        return "Me".to_string();
    }
    recipients
        .get(&item.author_id)
        .map(|r| r.display())
        .unwrap_or_else(|| format!("recipient_{}", item.author_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipient(id: &str, name: &str, number: Option<&str>, aci: &str) -> ParsedRecipient {
        ParsedRecipient {
            id: id.into(),
            identifier: number.map(str::to_string),
            display_name: Some(name.into()),
            aci: Some(aci.into()),
        }
    }

    /// A Signal mention is a `U+FFFC` in the body; it reads as the person's
    /// name and is searched by their number, else their ACI. An ACI the
    /// backup has no recipient for still names someone; a placeholder
    /// with none behind it stays as it was.
    #[test]
    fn a_mention_reads_as_its_name_and_is_found_by_its_handle() {
        let worf = "00000000-0000-4000-8000-00000000000a";
        let data = "00000000-0000-4000-8000-00000000000b";
        let stranger = "00000000-0000-4000-8000-00000000000c";
        let map: HashMap<String, ParsedRecipient> = [
            recipient("7", "Worf", Some("+12025550100"), worf),
            recipient("8", "Data", None, data),
        ]
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
        let inputs = Inputs::default();
        let by_aci: HashMap<&str, &str> = map
            .values()
            .filter_map(|r| Some((r.aci.as_deref()?, r.id.as_str())))
            .collect();
        let (text, mentioned) = with_mentions(
            inputs.lookup("recipients", &map),
            &by_aci,
            "\u{FFFC} and \u{FFFC}, then \u{FFFC} and \u{FFFC} again \u{FFFC}",
            &[
                Some(worf.into()),
                Some(data.into()),
                None,
                Some(stranger.into()),
                Some(worf.into()),
            ],
        );
        assert_eq!(
            text,
            format!("@Worf and @Data, then \u{FFFC} and @{stranger} again @Worf")
        );
        let handles: Vec<&str> = mentioned.iter().map(Handle::as_str).collect();
        assert_eq!(
            handles,
            [
                "tel:+12025550100",
                &format!("signal_aci:{data}"),
                &format!("signal_aci:{stranger}"),
            ]
        );
    }
}
