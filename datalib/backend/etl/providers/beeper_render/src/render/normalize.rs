//! Beeper's parsed render units, in the shape `chat-common` renders.
//!
//! Beeper bridges many upstreams, so its chats are grouped by network:
//! one `RenderProfile` per network, because the `grid_rows` taxonomy is
//! per-network too ("Signal Chat", "Google Chat Message") and so is the
//! composite `source_label`.

use std::collections::{BTreeMap, HashMap};

use datalib_contact_schema::{ContactKind, NormalizedContact, Photo};
use datalib_etl_chat_common::types::{
    AuthorKind, ItemKind, NamedPerson, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, NormalizedReaction, OrphanReactions, UpstreamRef,
};
use datalib_etl_chat_common::RenderProfile;
use datalib_etl_chat_common::TextFormat;
use datalib_etl_render::html::{escape_md_block, md_code_span};
use datalib_etl_render::inputs::Inputs;
use datalib_schema::providers::Provider;

use super::ids;
use super::parse::{Blob, DocRenderUnit, Event, ParsedBeeper, Person, Room};
use super::RENDER_VERSION;

/// Each avatar the blob store holds as an image, by blake3.
pub type Avatars = HashMap<String, Photo>;

/// Everything one network's chats need to render: the profile that
/// names its grid-row taxonomy, and the chats themselves.
pub struct NetworkChats {
    pub profile: RenderProfile,
    pub chats: Vec<NormalizedChat>,
}

/// A render unit whose document this run could not build, and why: a
/// `problems` row on that document rather than a skip nobody sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrendered {
    pub markdown_uuid: String,
    pub why: String,
}

/// Every room's chat, grouped by network.
pub fn to_networks(
    parsed: &ParsedBeeper,
    source_id: &str,
    avatars: &Avatars,
) -> (Vec<NetworkChats>, Vec<Unrendered>) {
    let mut by_network: BTreeMap<String, Vec<NormalizedChat>> = BTreeMap::new();
    let mut unrendered = Vec::new();

    for doc in &parsed.docs {
        // The parser fills the room map from the same store, so this
        // should not happen; the render unit fails with a `problems` row
        // on its document, and the rest of the run goes on.
        let Some(room) = parsed.rooms.get(&doc.room_uuid) else {
            unrendered.push(Unrendered {
                markdown_uuid: ids::room(source_id, &doc.room_uuid).uuid,
                why: format!(
                    "its events name room {}, which the parse did not read",
                    doc.room_uuid
                ),
            });
            continue;
        };
        let no_inputs = Inputs::default();
        let inputs = parsed.inputs.get(&doc.room_uuid).unwrap_or(&no_inputs);
        let people = parsed
            .people
            .get(&doc.room_uuid)
            .map(Vec::as_slice)
            .unwrap_or_default();
        by_network
            .entry(room.network.clone())
            .or_default()
            .push(to_chat(source_id, room, people, avatars, doc, inputs));
    }

    let networks = by_network
        .into_iter()
        .map(|(network, chats)| NetworkChats {
            profile: profile_for(&network),
            chats,
        })
        .collect();
    (networks, unrendered)
}

/// The bundle key for a room's document — matches `NormalizedChat::id`.
pub fn bundle_key(doc: &DocRenderUnit) -> String {
    doc.room_uuid.clone()
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
        chat_entity_kind: ids::KIND_ROOM,
        // Beeper is the one source whose upstream stamps are meaningful
        // below the second, and its `created_at` has always said so.
        stamp_precision: ids::STAMP_PRECISION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
        layout: datalib_etl_chat_common::Layout::Chat,
    }
}

fn to_chat(
    source_id: &str,
    room: &Room,
    people: &[Person],
    avatars: &Avatars,
    doc: &DocRenderUnit,
    inputs: &Inputs,
) -> NormalizedChat {
    let uuid_of: HashMap<&str, String> = doc
        .messages
        .iter()
        .map(|m| {
            let uuid = ids::event(source_id, &m.native_event_id, m.timestamp_ms).uuid;
            (m.native_event_id.as_str(), uuid)
        })
        .collect();
    let items: Vec<NormalizedChatItem> = doc
        .messages
        .iter()
        .map(|m| to_item(source_id, room, doc, m, &uuid_of))
        .collect();

    // Reactions whose target event is not in the store at all: never
    // downloaded, or in another room.
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
            reactions: rs.iter().map(|r| to_reaction(source_id, r)).collect(),
        })
        .collect();

    // A room with no name of its own is called by its other people.
    let named_after: Vec<NamedPerson> = match &room.title {
        Some(_) => Vec::new(),
        None => people
            .iter()
            .filter(|p| !p.is_self)
            .map(|p| NamedPerson {
                shown: p.label(),
                handle: p.handle.clone(),
            })
            .collect(),
    };
    let display = room
        .title
        .clone()
        .or_else(|| {
            let names: Vec<&str> = named_after.iter().map(|p| p.shown.as_str()).collect();
            (!names.is_empty()).then(|| names.join(", "))
        })
        .or_else(|| room.external_room_id.clone())
        .unwrap_or_else(|| room.native_room_id.clone());

    NormalizedChat {
        named_after,
        contacts: people
            .iter()
            .filter_map(|p| contact(source_id, p, avatars))
            .collect(),
        inputs: inputs.declared(),
        id: bundle_key(doc),
        containers: Vec::new(),
        filed_under: Vec::new(),
        chat_uuid: ids::room(source_id, &room.native_room_id).uuid,
        display,
        // `None`, so chat-common derives the same
        // "{source_label} · {display}" heading the other chat sources
        // get — here "Beeper:Signal · Bridge Crew".
        title: None,
        author: None,
        account: room.account_id.clone(),
        project: room.external_workspace_id.clone(),
        // The room's id is minted from its native (Matrix) id; the
        // bridge's own id stays on `project`'s neighbour columns.
        external_id: Some(room.native_room_id.clone()),
        upstream_account: None,
        source_url: room.source_url.clone(),
        org_uuid: None,
        org_name: None,
        render_units: vec![NormalizedDoc {
            markdown_uuid: ids::room(source_id, &room.native_room_id).uuid,
            source_ref: None,
            items,
            orphan_reactions,
        }],
    }
}

/// `uuid_of` is the room's messages' uuids by their event id, so a
/// reply finds the message it answers.
fn to_item(
    source_id: &str,
    room: &Room,
    doc: &DocRenderUnit,
    m: &Event,
    uuid_of: &HashMap<&str, String>,
) -> NormalizedChatItem {
    let reactions: Vec<NormalizedReaction> = doc
        .reactions_by_target
        .get(&m.native_event_id)
        .map(|rs| rs.iter().map(|r| to_reaction(source_id, r)).collect())
        .unwrap_or_default();

    let attachments: Vec<NormalizedAttachment> =
        m.blobs.iter().map(|b| to_attachment(m, b)).collect();

    let reply_to = m
        .reply_to_native_event_id
        .as_deref()
        .and_then(|id| uuid_of.get(id).cloned());
    let body = message_body(
        m.reply_to_native_event_id.as_deref(),
        m.text_content.as_deref(),
    );

    if m.is_hidden() {
        // Membership changes, encryption setup, transcript-exclude
        // marks: real history the desktop app suppresses. Rendered as
        // the small italic line every provider's system events get, and
        // kept out of the chat row's search text.
        return NormalizedChatItem {
            reply_to,
            message_uuid: ids::event(source_id, &m.native_event_id, m.timestamp_ms).uuid,
            author_handle: m.sender_handle.clone(),
            author_kind: AuthorKind::Named,
            author_display: m.sender_label.clone().unwrap_or_default(),
            date_ms: Some(m.timestamp_ms),
            date_offset_secs: None,
            text: None,
            kind: ItemKind::System,
            attachments: Vec::new(),
            reactions,
            labels: Vec::new(),
            system_note: Some(hidden_summary(m)),
            source_url: None,
            kind_label: Some(kind_for_message(&room.network, &m.event_type)),
            source_ref: Some(UpstreamRef::new(ids::KIND_EVENT, m.native_event_id.clone())),
            is_aside: false,
            branch: Vec::new(),
            unread: false,
            replies: None,
            recipients: Vec::new(),
            mentions: Vec::new(),
            problems: m.problems.clone(),
        };
    }

    NormalizedChatItem {
        reply_to,
        message_uuid: ids::event(source_id, &m.native_event_id, m.timestamp_ms).uuid,
        author_handle: m.sender_handle.clone(),
        author_kind: AuthorKind::Named,
        author_display: m.sender_label.clone().unwrap_or_default(),
        date_ms: Some(m.timestamp_ms),
        date_offset_secs: None,
        text: body,
        kind: if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        },
        attachments,
        reactions,
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_for_message(&room.network, &m.event_type)),
        source_ref: Some(UpstreamRef::new(ids::KIND_EVENT, m.native_event_id.clone())),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        replies: None,
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems: m.problems.clone(),
    }
}

/// The person as Beeper knows them, for a person with a handle: their
/// names, every number and address it lists, and their avatar where the
/// app had cached it.
fn contact(source_id: &str, p: &Person, avatars: &Avatars) -> Option<NormalizedContact> {
    let handle = p.handle.as_ref()?;
    let mut c = NormalizedContact::new(source_id, handle.as_str(), ContactKind::Person);
    c.names = p.names.clone();
    c.handles = p.handles.clone();
    c.photo = p.avatar.as_ref().and_then(|h| avatars.get(h)).cloned();
    c.account = p.is_self;
    Some(c)
}

fn to_attachment(m: &Event, b: &Blob) -> NormalizedAttachment {
    NormalizedAttachment {
        // chat-common fills this in once it has materialized the bytes.
        href: None,
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
        source_url: None,
        ref_id: b.blake3.clone(),
    }
}

fn to_reaction(source_id: &str, r: &Event) -> NormalizedReaction {
    NormalizedReaction {
        count: 1,
        // Beeper's own event_uuid already collapses sender+target+emoji
        // on the source side, so it is the reaction's identity.
        reaction_uuid: ids::event(source_id, &r.native_event_id, r.timestamp_ms).uuid,
        reactor_handle: r.sender_handle.clone(),
        reactor_kind: match r.sender_label {
            Some(_) => AuthorKind::Named,
            None => AuthorKind::Unknown,
        },
        reactor_display: r.sender_label.clone().unwrap_or_else(|| "?".into()),
        emoji: r.reaction_emoji.clone().unwrap_or_else(|| "?".into()),
        date_ms: Some(r.timestamp_ms),
        source_ref: Some(UpstreamRef::new(ids::KIND_EVENT, r.native_event_id.clone())),
    }
}

/// A message's markdown: what was typed, escaped, under a line naming
/// the message it replies to, by the id upstream gave it; the reply
/// edge is what links the two.
fn message_body(reply_to: Option<&str>, text: Option<&str>) -> Option<String> {
    let reply_line = reply_to.map(|id| format!("> ↪ in reply to {}", md_code_span(id)));
    let typed = text.filter(|s| !s.is_empty()).map(escape_md_block);
    match (reply_line, typed) {
        (Some(r), Some(t)) => Some(format!("{r}\n\n{t}")),
        (r, t) => r.or(t),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A render unit whose room the parse did not read was skipped with a
    /// log line; its document now carries why it was not rendered. A
    /// render unit beside it whose room is there renders.
    #[test]
    fn a_render_unit_with_no_room_is_unrendered() {
        let render_unit = |room_uuid: &str| DocRenderUnit {
            room_uuid: room_uuid.into(),
            messages: Vec::new(),
            reactions_by_target: BTreeMap::new(),
        };
        let bridge = Room {
            room_uuid: "!bridge:beeper.local".into(),
            network: "signal".into(),
            native_room_id: "!bridge:beeper.local".into(),
            external_room_id: None,
            external_workspace_id: None,
            account_id: None,
            title: Some("Bridge".into()),
            is_dm: false,
            source_url: None,
        };
        let parsed = ParsedBeeper {
            rooms: [(bridge.room_uuid.clone(), bridge)].into(),
            docs: vec![
                render_unit("!bridge:beeper.local"),
                render_unit("!enterprise:beeper.local"),
            ],
            ..ParsedBeeper::default()
        };
        let (networks, unrendered) = to_networks(&parsed, "beeper", &Avatars::new());
        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].chats.len(), 1);
        assert_eq!(unrendered.len(), 1);
        assert_eq!(
            unrendered[0].markdown_uuid,
            ids::room("beeper", "!enterprise:beeper.local").uuid
        );
    }

    #[test]
    fn a_message_in_markup_renders_escaped_under_its_reply_line() {
        assert_eq!(
            message_body(Some("$evt`1"), Some("<script>x</script> & co")).as_deref(),
            Some("> ↪ in reply to `` $evt`1 ``\n\n&lt;script&gt;x&lt;/script&gt; &amp; co")
        );
        assert_eq!(
            message_body(None, Some("# Engage")).as_deref(),
            Some("\\# Engage")
        );
        assert_eq!(message_body(None, Some("")), None);
    }
}
