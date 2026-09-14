//! Signal's parsed buckets, in the shape `chat-common` renders.
//!
//! Signal used to write its own markdown — a bulleted list with a raw
//! ISO stamp per line — and its own grid rows. Everything below turns
//! its parse output into the normalized types instead, so one renderer
//! serves it and the seven other chat sources alike.

use std::collections::HashMap;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use datalib_etl_render::inputs::{Inputs, Lookup};

use super::parse::{ParsedChat, ParsedChatItem, ParsedRecipient, ParsedSignal};
use super::{signal_chat_uuid, signal_markdown_uuid, signal_message_uuid};

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
        let chat_uuid = signal_chat_uuid(source_id, &chat.id);
        let bundle_key = format!("{}#{}", chat.id, doc.period_key);
        // Recipients are declared as they are looked up, on the chat's
        // inputs — every period of the chat shares one declaration.
        let no_inputs = Inputs::default();
        let inputs = parsed.inputs.get(&chat.id).unwrap_or(&no_inputs);
        let recipients = inputs.lookup("recipients", &parsed.recipients);

        let items: Vec<NormalizedChatItem> = doc
            .items
            .iter()
            // An item with neither text nor an attachment is a
            // ChatUpdate or a sticker: nothing a transcript can show.
            // The old renderer skipped these too, but kept counting
            // them, so `message_index` disagreed with the rendered
            // order whenever one appeared.
            .filter(|i| i.text.is_some() || !i.attachments.is_empty())
            .map(|item| to_item(recipients, chat, item, source_id))
            .collect();

        chats.push(NormalizedChat {
            path_prefix: None,
            id: bundle_key.clone(),
            chat_uuid: chat_uuid.clone(),
            display: recipient_display(recipients, chat),
            // `None`, so chat-common derives the familiar
            // "Signal · {recipient}" heading rather than us restating it.
            title: None,
            author: None,
            account: None,
            project: None,
            external_id: None,
            upstream_scope: None,
            // Signal Android backups expose no per-thread web URL.
            source_url: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![NormalizedDoc {
                orphan_reactions: Vec::new(),
                period_key: doc.period_key.clone(),
                markdown_uuid: signal_markdown_uuid(&chat_uuid, &doc.period_key),
                items,
            }],
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

    NormalizedChatItem {
        message_uuid: signal_message_uuid(source_id, &chat.id, &item.author_id, item.date_sent),
        author_id: item.author_id.clone(),
        author_display: author_display(recipients, item),
        date_ms: Some(item.date_sent),
        text: item.text.clone(),
        kind: if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        },
        attachments,
        reactions: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
    }
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
