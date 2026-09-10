//! Beeper's parsed buckets, in the shape `chat-common` renders.
//!
//! Beeper bridges many upstreams, so its chats are grouped by network:
//! one `RenderProfile` per network, because the `grid_rows` taxonomy is
//! per-network too ("Signal Chat", "Google Chat Message") and so is the
//! composite `source_label`.

use std::collections::BTreeMap;

use datalib_etl_chat_common::render::ENTITY_KIND_CONVERSATION;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction, OrphanReactions,
};
use datalib_etl_chat_common::{RenderProfile, WhenTsPrecision};
use datalib_schema::providers::Provider;

use super::parse::{Blob, DocBucket, Event, ParsedBeeper, Room};
use super::{beeper_markdown_uuid, RENDER_VERSION};

/// Everything one network's chats need to render: the profile that
/// names its grid-row taxonomy, and the chats themselves.
pub struct NetworkChats {
    pub profile: RenderProfile,
    pub chats: Vec<NormalizedChat>,
}

/// Group every bucket by network, then by room.
///
/// One `NormalizedChat` per *bucket* rather than per room, so a room
/// spanning three periods yields three chats over one `chat_uuid`. That
/// keeps the chat-level grid row per-document, which is what beeper has
/// always emitted, and it keeps each bucket's attachments keyed apart.
pub fn to_networks(parsed: &ParsedBeeper) -> Vec<NetworkChats> {
    let mut by_network: BTreeMap<String, Vec<NormalizedChat>> = BTreeMap::new();

    for doc in &parsed.docs {
        let Some(room) = parsed.rooms.get(&doc.room_uuid) else {
            // The parser populates the room map from the same store, so
            // this should not happen; log and drop the bucket rather
            // than abort the whole render pass.
            tracing::warn!(
                event = "beeper_render_missing_room",
                room_uuid = %doc.room_uuid,
                "bucket names a room the parse did not produce; skipping"
            );
            continue;
        };
        by_network
            .entry(room.network.clone())
            .or_default()
            .push(to_chat(room, doc));
    }

    by_network
        .into_iter()
        .map(|(network, chats)| NetworkChats {
            profile: profile_for(&network),
            chats,
        })
        .collect()
}

/// The bundle key for a bucket — matches `NormalizedChat::id`.
pub fn bundle_key(doc: &DocBucket) -> String {
    format!("{}#{}", doc.room_uuid, doc.period_key)
}

pub fn profile_for(network: &str) -> RenderProfile {
    let label = network_label(network);
    RenderProfile {
        provider: Provider::Beeper,
        // Composite `source_label` carries both routing layers:
        //   "Beeper:Signal", "Beeper:Google Chat", "Beeper:WhatsApp", …
        // Downstream queries can do `LIKE 'Beeper:%'` to pull everything
        // that came through this provider, or `LIKE '%:Signal'` to grab
        // Signal regardless of which extractor delivered it.
        source_label: format!("Beeper:{label}"),
        chat_kind: format!("{label} Chat"),
        message_kind: format!("{label} Message"),
        reaction_kind: format!("{label} Reaction"),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        // Beeper is the one source whose upstream stamps are meaningful
        // below the second, and its `when_ts` has always said so.
        when_ts_precision: WhenTsPrecision::Millis,
        render_version: RENDER_VERSION,
    }
}

fn to_chat(room: &Room, doc: &DocBucket) -> NormalizedChat {
    let items: Vec<NormalizedChatItem> =
        doc.messages.iter().map(|m| to_item(room, doc, m)).collect();

    // Reactions whose target is not one of this bucket's messages.
    //
    // Parse already routes a reaction to its *target's* period bucket,
    // resolving the target against every event in the store — so this
    // is not "the message is in another document". It is the case
    // nothing can place: the target event is not in the store at all,
    // and parse fell back to filing the reaction under its own period.
    let known: std::collections::HashSet<&str> = doc
        .messages
        .iter()
        .map(|m| m.native_event_id.as_str())
        .collect();
    let orphan_reactions: Vec<OrphanReactions> = doc
        .reactions_by_target
        .iter()
        .filter(|(target, _)| !known.contains(target.as_str()))
        .map(|(target, rs)| OrphanReactions {
            target_native_id: target.clone(),
            reactions: rs.iter().map(to_reaction).collect(),
        })
        .collect();

    NormalizedChat {
        id: bundle_key(doc),
        chat_uuid: room.room_uuid.clone(),
        display: room
            .title
            .clone()
            .or_else(|| room.external_room_id.clone())
            .unwrap_or_else(|| room.native_room_id.clone()),
        // `None`, so chat-common derives the same
        // "{source_label} · {display}" heading the other chat sources
        // get — here "Beeper:Signal · Bridge Crew". The old
        // "signal · 2024-03" restated the period the heading already
        // ends with.
        title: None,
        account: room.account_id.clone(),
        project: room.external_workspace_id.clone(),
        external_id: room.external_room_id.clone(),
        upstream_scope: None,
        source_url: None,
        org_uuid: None,
        org_name: None,
        // A Beeper stanza bridges several networks, so they stay apart
        // on disk: `render_markdown/<network>/<room_uuid>/<period>.md`.
        path_prefix: Some(room.network.clone()),
        buckets: vec![NormalizedDoc {
            period_key: doc.period_key.clone(),
            markdown_uuid: beeper_markdown_uuid(&room.room_uuid, &doc.period_key),
            items,
            orphan_reactions,
        }],
    }
}

fn to_item(room: &Room, doc: &DocBucket, m: &Event) -> NormalizedChatItem {
    let reactions: Vec<NormalizedReaction> = doc
        .reactions_by_target
        .get(&m.native_event_id)
        .map(|rs| rs.iter().map(to_reaction).collect())
        .unwrap_or_default();

    let attachments: Vec<NormalizedAttachment> =
        m.blobs.iter().map(|b| to_attachment(m, b)).collect();

    // A reply keeps the bridge id it points at. We do not link it to the
    // target's anchor: the native↔matrix id bridge makes that fiddly
    // when the target lives in a different period file.
    let reply_line = m
        .reply_to_native_event_id
        .as_deref()
        .map(|id| format!("> ↪ in reply to `{id}`"));
    let body = match (
        reply_line,
        m.text_content.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(r), Some(t)) => Some(format!("{r}\n\n{t}")),
        (Some(r), None) => Some(r),
        (None, t) => t.map(str::to_string),
    };

    if m.is_hidden() {
        // Membership changes, encryption setup, transcript-exclude
        // marks: real history the desktop app suppresses. Rendered as
        // the small italic line every provider's system events get, and
        // kept out of the chat row's search text.
        return NormalizedChatItem {
            message_uuid: m.event_uuid.clone(),
            author_id: m.sender_uuid.clone().unwrap_or_default(),
            author_display: m.sender_label.clone().unwrap_or_default(),
            date_ms: Some(m.timestamp_ms),
            text: None,
            kind: ItemKind::System,
            attachments: Vec::new(),
            reactions,
            system_note: Some(hidden_summary(m)),
            source_url: None,
            kind_label: Some(kind_for_message(&room.network, &m.event_type)),
            source_ref: None,
            is_aside: false,
        };
    }

    NormalizedChatItem {
        message_uuid: m.event_uuid.clone(),
        author_id: m.sender_uuid.clone().unwrap_or_default(),
        author_display: m.sender_label.clone().unwrap_or_default(),
        date_ms: Some(m.timestamp_ms),
        text: body,
        kind: if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        },
        attachments,
        reactions,
        system_note: None,
        source_url: None,
        kind_label: Some(kind_for_message(&room.network, &m.event_type)),
        source_ref: None,
        is_aside: false,
    }
}

fn to_attachment(m: &Event, b: &Blob) -> NormalizedAttachment {
    NormalizedAttachment {
        // chat-common fills this in once it has materialized the bytes.
        rel_path: None,
        file_name: Some(b.slot.clone()),
        // The event type is what beeper knows an attachment *is*; fall
        // back to it so an untyped image still renders inline.
        mime_type: b
            .content_type
            .clone()
            .or_else(|| match m.event_type.as_str() {
                "IMAGE" => Some("image/*".to_string()),
                "VIDEO" => Some("video/*".to_string()),
                "AUDIO" | "VOICE" => Some("audio/*".to_string()),
                _ => None,
            }),
        byte_len: b.byte_len,
        source_url: b.source_url.clone(),
        ref_id: b.blake3.clone(),
    }
}

fn to_reaction(r: &Event) -> NormalizedReaction {
    NormalizedReaction {
        // Beeper's own event_uuid already collapses sender+target+emoji
        // on the source side, so it is the reaction's identity.
        reaction_uuid: r.event_uuid.clone(),
        reactor_display: r.sender_label.clone().unwrap_or_else(|| "?".into()),
        emoji: r.reaction_emoji.clone().unwrap_or_else(|| "?".into()),
        date_ms: Some(r.timestamp_ms),
        source_ref: None,
    }
}

fn hidden_summary(m: &Event) -> String {
    let kind = m.event_type.to_lowercase();
    match m.text_content.as_deref().filter(|s| !s.is_empty()) {
        Some(t) => format!("{kind}: {t}"),
        None => kind,
    }
}

pub fn kind_for_message(network: &str, ev_type: &str) -> String {
    let label = network_label(network);
    match ev_type {
        "TEXT" | "NOTICE" => format!("{label} Message"),
        "IMAGE" => format!("{label} Image"),
        "VIDEO" => format!("{label} Video"),
        "FILE" => format!("{label} File"),
        "AUDIO" | "VOICE" => format!("{label} Audio"),
        "MEMBERSHIP" => format!("{label} Membership"),
        // Distinct `Hidden` kind so consumers can filter cheaply
        // (downstream sees `kind = "Signal Hidden"` etc.). Same pattern
        // as MEMBERSHIP — taxonomy parity matters for search facets.
        "HIDDEN" => format!("{label} Hidden"),
        other => format!("{label} {other}"),
    }
}

pub fn network_label(network: &str) -> &str {
    match network {
        "signal" => "Signal",
        "googlechat" => "Google Chat",
        "slack" => "Slack",
        "whatsapp" => "WhatsApp",
        "imessage" => "iMessage",
        "telegram" => "Telegram",
        "discord" => "Discord",
        "linkedin" => "LinkedIn",
        "twitter" => "Twitter",
        "instagram" => "Instagram",
        "facebook" => "Facebook",
        "sms" => "SMS",
        other => other,
    }
}
