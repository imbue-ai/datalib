//! A Discord conversation as the shared chat model: one document per
//! channel, thread, forum post or direct message.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact, Photo};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{ChatRenderer, RenderProfile};
use datalib_etl_chat_common::types::{
    AuthorKind, ItemKind, NamedPerson, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, NormalizedReaction, UpstreamRef,
};
use datalib_etl_chat_common::{render_paged, Page, PageEnd, PagedChats, TextFormat};
use datalib_etl_discord::ingest::db::files_of;
use datalib_etl_discord::ingest::schema_raw::{is_webhook, CHANNELS, GUILDS, ROLES, USERS};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{escape_md_inline, md_quote};
use datalib_etl_render::message::chip_link;
use datalib_etl_render::pages::{PageSize, RenderUnitPages};
use datalib_handle::Handle;
use datalib_schema::data_nodes::DataNodeRow;
use datalib_schema::problems::{Problem, Reason, Severity};
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::embeds;
use crate::ids;
use crate::markdown::{mentioned, to_commonmark, Labels};
use crate::store::{Channel, Conversation, Message, Shared, Store};

/// Bump when the rendered output changes in a way that must invalidate
/// stale documents.
pub const RENDER_VERSION: u32 = 1;

/// Conversations read at once: a busy channel is a great many messages,
/// so far fewer than the shared page of render units.
const CONVERSATIONS_PER_PAGE: usize = 20;

/// How much of the message a reply answers is quoted above it.
const REPLY_QUOTE_MAX: usize = 120;

pub fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Discord,
        source_label: "Discord".to_string(),
        chat_kind: "Discord Chat".to_string(),
        message_kind: "Discord Message".to_string(),
        reaction_kind: "Discord Reaction".to_string(),
        chat_entity_kind: ids::KIND_CONVERSATION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
        layout: datalib_etl_chat_common::Layout::Chat,
    }
}

/// Renders every conversation `store` hands out, a page at a time;
/// `on_page` hears how each page ended before the next is read.
pub fn render_store(
    store: &Store,
    source_id: &str,
    size: PageSize,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    on_page: &mut dyn FnMut(PageEnd) -> Result<()>,
) -> Result<()> {
    tracing::info!(
        source = source_id,
        conversations = store.conversations,
        cold_start = store.render.is_none(),
        "[render] discord dolt_diff scan"
    );
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        source_id,
        store.conversations,
        progress,
        on_doc_complete,
    );
    let pages = store.pages(size.render_units.min(CONVERSATIONS_PER_PAGE));
    let chats = Chats { store, source_id };
    render_paged(&chats, pages, size.bytes, &mut renderer, on_page).context("discord render")
}

struct Chats<'a> {
    store: &'a Store,
    source_id: &'a str,
}

impl PagedChats for Chats<'_> {
    type Conversation = Conversation;

    fn next_page(&self, pages: &mut RenderUnitPages) -> Result<Option<Page<Conversation>>> {
        let page = self.store.next_page(pages)?;
        Ok(page.map(|mut page| {
            for id in &mut page.looked_at {
                *id = ids::conversation(self.source_id, id).uuid;
            }
            page
        }))
    }

    fn attachment_bytes(&self, c: &Conversation) -> u64 {
        c.attachment_bytes()
    }

    fn load_blobs(&self, batch: &[&Conversation]) -> Result<Vec<Conversation>> {
        self.store.load_blobs(batch)
    }

    fn chat(&self, c: &Conversation) -> NormalizedChat {
        build_chat(&self.store.shared, c, self.source_id)
    }

    fn blobs<'c>(&self, c: &'c Conversation) -> &'c BlobBundle {
        &c.blobs
    }
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn link(guild: Option<&str>, channel: &str, message: Option<&str>) -> String {
    let guild = guild.unwrap_or("@me");
    match message {
        Some(m) => format!("https://discord.com/channels/{guild}/{channel}/{m}"),
        None => format!("https://discord.com/channels/{guild}/{channel}"),
    }
}

/// What building one conversation's messages looks up.
struct Building<'a> {
    source_id: &'a str,
    shared: &'a Shared,
    labels: Labels<'a>,
    channel: Option<&'a Channel>,
    channel_id: &'a str,
    /// The conversation's messages by their ids: what a reply can quote.
    held: HashMap<&'a str, &'a Message>,
}

fn build_chat(shared: &Shared, c: &Conversation, source_id: &str) -> NormalizedChat {
    let channels = c.inputs.lookup(CHANNELS, &shared.channels);
    let guilds = c.inputs.lookup(GUILDS, &shared.guilds);
    let labels = Labels {
        users: c.inputs.lookup(USERS, &shared.labels),
        channels: c.inputs.lookup(CHANNELS, &shared.channel_names),
        roles: c.inputs.lookup(ROLES, &shared.roles),
    };
    let channel = channels.get(&c.channel_id);
    let parent = channel
        .and_then(|ch| ch.parent_id.as_deref())
        .and_then(|id| channels.get(id));
    let guild_id = channel.and_then(|ch| ch.guild_id.as_deref());
    let guild_name = guild_id.and_then(|g| guilds.get(g)).cloned();
    let b = Building {
        source_id,
        shared,
        labels,
        channel,
        channel_id: &c.channel_id,
        held: c.messages.iter().map(|m| (m.id.as_str(), m)).collect(),
    };
    let named_after = match channel {
        Some(ch) if ch.guild_id.is_none() && ch.name.is_none() => b.recipients(ch),
        _ => Vec::new(),
    };
    let display = b.display(&named_after);
    let conversation = ids::conversation(source_id, &c.channel_id);
    let place = match (channel, parent) {
        (Some(ch), Some(parent)) if ch.is_thread => Some(parent),
        _ => channel,
    };
    NormalizedChat {
        named_after,
        id: conversation.uuid.clone(),
        containers: containers(source_id, guild_id, guild_name.clone(), place),
        filed_under: Vec::new(),
        chat_uuid: conversation.uuid.clone(),
        display,
        title: None,
        author: None,
        account: shared.account.as_ref().and_then(|(_, label)| label.clone()),
        project: None,
        external_id: Some(conversation.natural_key),
        source_url: Some(link(guild_id, &c.channel_id, None)),
        upstream_account: None,
        org_uuid: guild_id.map(String::from),
        org_name: guild_name,
        render_units: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            markdown_uuid: conversation.uuid,
            source_ref: None,
            items: c.messages.iter().map(|m| b.item(m)).collect(),
        }],
        contacts: b.contacts(&c.messages, &c.inputs),
        inputs: c.inputs.declared(),
    }
}

/// The server, then the channel: both places a person browses. A thread
/// is in its channel. A direct message is in neither.
fn containers(
    source_id: &str,
    guild_id: Option<&str>,
    guild_name: Option<String>,
    channel: Option<&Channel>,
) -> Vec<DataNodeRow> {
    let (Some(guild_id), Some(channel)) = (guild_id, channel) else {
        return Vec::new();
    };
    let server = ids::server(source_id, guild_id);
    let place = ids::channel(source_id, &channel.id);
    let name = channel.name.as_ref().map(|n| format!("#{n}"));
    vec![
        DataNodeRow::new("", server.uuid, source_id, "Discord Server")
            .named(guild_name)
            .upstream(server.entity_kind, server.natural_key, None)
            .place(),
        DataNodeRow::new("", place.uuid, source_id, "Discord Channel")
            .named(name)
            .upstream(place.entity_kind, place.natural_key, None)
            .place(),
    ]
}

impl Building<'_> {
    fn label(&self, user_id: &str) -> String {
        self.labels
            .users
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string())
    }

    fn recipients(&self, channel: &Channel) -> Vec<NamedPerson> {
        channel
            .recipients
            .iter()
            .map(|id| NamedPerson {
                shown: self.label(id),
                handle: Handle::discord(id),
            })
            .collect()
    }

    /// `#bridge` for a channel, a thread's or a group's own name, and the
    /// people of a direct message with none.
    fn display(&self, named_after: &[NamedPerson]) -> String {
        let name = self.channel.and_then(|c| c.name.as_deref());
        let in_server = self.channel.is_some_and(|c| c.guild_id.is_some());
        let is_thread = self.channel.is_some_and(|c| c.is_thread);
        match name {
            Some(name) if in_server && !is_thread => format!("#{name}"),
            Some(name) => name.to_string(),
            None if !named_after.is_empty() => {
                let names: Vec<&str> = named_after.iter().map(|p| p.shown.as_str()).collect();
                format!("@{}", names.join(", "))
            }
            None => format!("#{}", self.channel_id),
        }
    }

    fn item(&self, m: &Message) -> NormalizedChatItem {
        let raw = &m.raw;
        let id = ids::message(self.source_id, &m.id);
        let (author_display, author_kind, author_handle) = self.author(raw);
        let kind = raw.get("type").and_then(Value::as_i64).unwrap_or(0);
        let mut problems = Vec::new();
        let note = match system_note(kind, raw, self) {
            System::Said => None,
            System::Note(note) => Some(format!("{author_display} {note}")),
            System::Unknown => {
                problems.push(
                    Problem::explained(
                        Reason::UncoveredType,
                        Some("type".to_string()),
                        &format!("a message of type {kind}, shown as its text alone"),
                    )
                    .severity(Severity::Info),
                );
                None
            }
        };
        let content = text(raw, "content").unwrap_or("");
        let reply = self.replied_to(raw, kind);
        let body = match &note {
            Some(_) => String::new(),
            None => {
                let mut parts: Vec<String> = Vec::new();
                parts.extend(reply.and_then(|r| self.reply_quote(r)));
                parts.push(to_commonmark(content.trim_end(), self.labels));
                parts.extend(embeds::draw(raw, self.labels));
                parts.retain(|p| !p.trim().is_empty());
                parts.join("\n\n")
            }
        };
        let attachments: Vec<NormalizedAttachment> =
            files_of(raw).into_iter().map(attachment).collect();
        let item_kind = match (&note, attachments.is_empty()) {
            (Some(_), _) => ItemKind::System,
            (None, true) => ItemKind::Text,
            (None, false) => ItemKind::Attachment,
        };
        let guild = self.channel.and_then(|c| c.guild_id.as_deref());
        NormalizedChatItem {
            message_uuid: id.uuid.clone(),
            reply_to: reply.map(|r| ids::message(self.source_id, &r.id).uuid),
            recipients: Vec::new(),
            mentions: mentioned(content),
            author_handle,
            author_display,
            author_kind,
            date_ms: ids::ms(&m.id),
            // A snowflake names an instant and no clock.
            date_offset_secs: None,
            text: (!body.is_empty()).then_some(body),
            kind: item_kind,
            attachments,
            reactions: self.reactions(m),
            labels: Vec::new(),
            system_note: note,
            source_url: Some(link(guild, self.channel_id, Some(&m.id))),
            kind_label: None,
            source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
            is_aside: false,
            branch: Vec::new(),
            unread: false,
            replies: None,
            problems,
        }
    }

    /// A webhook posts under a name of its own; anyone else is their
    /// profile.
    fn author(&self, raw: &Value) -> (String, AuthorKind, Option<Handle>) {
        let author = raw.get("author").unwrap_or(&Value::Null);
        if is_webhook(raw) {
            return match text(author, "username") {
                Some(name) => (name.to_string(), AuthorKind::Named, None),
                None => ("Unknown".to_string(), AuthorKind::Unknown, None),
            };
        }
        match text(author, "id") {
            Some(id) => (self.label(id), AuthorKind::Named, Handle::discord(id)),
            None => ("Unknown".to_string(), AuthorKind::Unknown, None),
        }
    }

    /// The message a reply answers, where this conversation holds it.
    fn replied_to(&self, raw: &Value, kind: i64) -> Option<&Message> {
        if !matches!(kind, 19 | 23) {
            return None;
        }
        let reference = raw.get("message_reference")?;
        if text(reference, "channel_id").is_some_and(|c| c != self.channel_id) {
            return None;
        }
        self.held.get(text(reference, "message_id")?).copied()
    }

    /// One line of what a reply answers, as Discord shows it above.
    fn reply_quote(&self, replied: &Message) -> Option<String> {
        let (author, _, handle) = self.author(&replied.raw);
        let who = match handle {
            Some(h) => chip_link(&author, &h),
            None => escape_md_inline(&author),
        };
        let content = text(&replied.raw, "content").unwrap_or("");
        let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let mut said: String = first.chars().take(REPLY_QUOTE_MAX).collect();
        if said.len() < first.len() {
            said.push('…');
        }
        let said = to_commonmark(&said, self.labels);
        let said = said.lines().next().unwrap_or("").trim().to_string();
        Some(md_quote(format!("↪ {who} {said}").trim_end()))
    }

    /// One reaction for the account's own, and one for everyone else's
    /// together: Discord counts who reacted and names nobody.
    fn reactions(&self, m: &Message) -> Vec<NormalizedReaction> {
        let mut out = Vec::new();
        let listed = m.raw.get("reactions").and_then(Value::as_array);
        for r in listed.into_iter().flatten() {
            let emoji = r.get("emoji").unwrap_or(&Value::Null);
            let name = text(emoji, "name").unwrap_or("?");
            let (key, shown) = match text(emoji, "id") {
                Some(id) => (id.to_string(), format!(":{name}:")),
                None => (name.to_string(), name.to_string()),
            };
            let count = r.get("count").and_then(Value::as_u64).unwrap_or(1);
            let mine = r.get("me").and_then(Value::as_bool) == Some(true)
                || r.get("me_burst").and_then(Value::as_bool) == Some(true);
            let mut push = |who: &str, display: String, kind, handle, count: u64| {
                let id = ids::reaction(self.source_id, &m.id, &key, who);
                out.push(NormalizedReaction {
                    reaction_uuid: id.uuid,
                    reactor_handle: handle,
                    reactor_display: display,
                    reactor_kind: kind,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                    emoji: shown.clone(),
                    date_ms: ids::ms(&m.id),
                    source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
                });
            };
            if mine {
                let me = self.shared.account.as_ref().map(|(id, _)| id.as_str());
                let display = me.map_or_else(|| "Me".to_string(), |id| self.label(id));
                let handle = me.and_then(Handle::discord);
                let kind = match handle {
                    Some(_) => AuthorKind::Named,
                    None => AuthorKind::Account,
                };
                push("me", display, kind, handle, 1);
            }
            let others = count.saturating_sub(u64::from(mine));
            if others > 0 {
                let display = match (mine, others) {
                    (true, 1) => "1 other".to_string(),
                    (true, n) => format!("{n} others"),
                    (false, 1) => "1 person".to_string(),
                    (false, n) => format!("{n} people"),
                };
                push("", display, AuthorKind::Unknown, None, others);
            }
        }
        out
    }

    /// Each author as Discord describes them.
    fn contacts(
        &self,
        messages: &[Message],
        inputs: &datalib_etl_render::inputs::Inputs,
    ) -> Vec<NormalizedContact> {
        let users = inputs.lookup(USERS, &self.shared.users);
        let account = self.shared.account.as_ref();
        let me = account.map(|(id, _)| id.as_str());
        // Discord tells an account its own address, and nobody else's.
        let my_address = account
            .and_then(|(_, label)| label.as_deref())
            .and_then(Handle::email);
        let mut seen: HashSet<&str> = HashSet::new();
        messages
            .iter()
            .filter(|m| !is_webhook(&m.raw))
            .filter_map(|m| text(m.raw.get("author")?, "id"))
            .filter(|id| seen.insert(id))
            .filter_map(|id| {
                let user = users.get(id)?;
                let handle = Handle::discord(id)?;
                let mut contact =
                    NormalizedContact::new(self.source_id, handle.as_str(), ContactKind::Person);
                for name in [&user.global_name, &user.username].into_iter().flatten() {
                    if !contact.names.contains(name) {
                        contact.names.push(name.clone());
                    }
                }
                contact.handles.push(ContactHandle::of(handle));
                contact.photo = user.avatar.as_ref().map(|hash| {
                    Photo::Url(format!(
                        "https://cdn.discordapp.com/avatars/{id}/{hash}.png"
                    ))
                });
                contact.account = Some(id) == me;
                if contact.account {
                    contact
                        .handles
                        .extend(my_address.clone().map(ContactHandle::of));
                }
                Some(contact)
            })
            .collect()
    }
}

fn attachment(file: &Value) -> NormalizedAttachment {
    NormalizedAttachment {
        href: None,
        file_name: text(file, "filename").map(String::from),
        mime_type: text(file, "content_type").map(String::from),
        byte_len: file.get("size").and_then(Value::as_i64),
        // Discord's link is signed and runs out in a day.
        source_url: None,
        ref_id: text(file, "id").map(String::from),
    }
}

enum System {
    /// Something a person or an app said.
    Said,
    /// What happened, after the name of who did it.
    Note(String),
    Unknown,
}

/// What each of Discord's message types is (its API reference, "Message
/// Types").
fn system_note(kind: i64, raw: &Value, b: &Building<'_>) -> System {
    let content = text(raw, "content").unwrap_or("");
    let named = |id: Option<&str>| id.map(|id| b.label(id)).unwrap_or_default();
    let mention = || {
        let first = raw.get("mentions").and_then(Value::as_array)?.first()?;
        text(first, "id")
    };
    let note = match kind {
        0 | 19 | 20 | 23 | 24 | 46 => return System::Said,
        1 => format!("added {}", named(mention())),
        2 => {
            let who = mention();
            let author = raw.get("author").and_then(|a| text(a, "id"));
            match who == author {
                true => "left".to_string(),
                false => format!("removed {}", named(who)),
            }
        }
        3 => "started a call".to_string(),
        4 => format!("renamed the conversation to {content}"),
        5 => "changed the conversation's icon".to_string(),
        6 => "pinned a message".to_string(),
        7 => "joined the server".to_string(),
        8..=11 => "boosted the server".to_string(),
        12 => format!("followed {content} into this channel"),
        14..=17 => "noted a change in the server's discovery listing".to_string(),
        18 => format!("started a thread: {content}"),
        21 => "started this thread from a message".to_string(),
        22 => "was reminded to invite people".to_string(),
        25 | 26 => "noted a subscription".to_string(),
        27 => format!("started a stage: {content}"),
        28 => format!("ended a stage: {content}"),
        29 => "became a speaker".to_string(),
        31 => format!("changed the stage's topic to {content}"),
        32 => "noted an app subscription".to_string(),
        36..=39 => "noted a change in the server's safety settings".to_string(),
        44 => "noted a purchase".to_string(),
        _ => return System::Unknown,
    };
    System::Note(note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::User;
    use datalib_etl_render::inputs::Inputs;
    use serde_json::json;

    const PICARD: &str = "1701";
    const RIKER: &str = "1702";

    fn user(name: &str) -> User {
        User {
            username: Some(name.to_lowercase()),
            global_name: Some(name.to_string()),
            avatar: Some("a1b2".to_string()),
            bot: false,
        }
    }

    fn shared() -> Shared {
        let channel = |id: &str, name: Option<&str>, guild: Option<&str>| Channel {
            id: id.to_string(),
            name: name.map(String::from),
            guild_id: guild.map(String::from),
            ..Default::default()
        };
        let mut thread = channel("73", Some("Warp core"), Some("70"));
        thread.parent_id = Some("71".into());
        thread.is_thread = true;
        let mut dm = channel("90", None, None);
        dm.recipients = vec![RIKER.to_string()];
        let mut s = Shared {
            account: Some((PICARD.into(), Some("picard@enterprise.starfleet".into()))),
            guilds: [("70".to_string(), "USS Enterprise".to_string())].into(),
            ..Default::default()
        };
        for c in [channel("71", Some("bridge"), Some("70")), thread, dm] {
            s.channels.insert(c.id.clone(), c);
        }
        for (id, name) in [(PICARD, "Jean-Luc Picard"), (RIKER, "Will Riker")] {
            s.users.insert(id.to_string(), user(name));
            s.labels.insert(id.to_string(), name.to_string());
        }
        s
    }

    fn message(id: &str, author: &str, content: &str) -> Value {
        json!({"id": id, "type": 0, "content": content, "author": {"id": author}})
    }

    fn chat(channel: &str, messages: Vec<Value>) -> NormalizedChat {
        let messages = messages
            .into_iter()
            .map(|raw| Message {
                key: format!("{channel}#{}", raw["id"].as_str().unwrap()),
                id: raw["id"].as_str().unwrap().to_string(),
                raw,
            })
            .collect();
        let c = Conversation {
            channel_id: channel.to_string(),
            messages,
            blobs: BlobBundle::default(),
            inputs: Inputs::default(),
        };
        build_chat(&shared(), &c, "src")
    }

    fn items(channel: &str, messages: Vec<Value>) -> Vec<NormalizedChatItem> {
        chat(channel, messages).render_units.remove(0).items
    }

    #[test]
    fn a_channel_is_in_its_server_and_a_thread_in_its_channel() {
        let bridge = chat("71", vec![message("1", RIKER, "Report.")]);
        assert_eq!(bridge.display, "#bridge");
        assert_eq!(bridge.org_name.as_deref(), Some("USS Enterprise"));
        let names: Vec<_> = bridge.containers.iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            names,
            [Some("USS Enterprise".to_string()), Some("#bridge".into())]
        );
        let thread = chat("73", vec![message("2", RIKER, "Breach.")]);
        assert_eq!(thread.display, "Warp core");
        assert_eq!(thread.containers[1].uuid, bridge.containers[1].uuid);
        assert_ne!(thread.chat_uuid, bridge.chat_uuid);
        // Both the channel and its parent are read, so a rename renders it.
        let read: Vec<String> = thread.inputs.iter().map(|i| format!("{i:?}")).collect();
        assert!(read.iter().any(|i| i.contains("\"71\"")), "{read:?}");
    }

    #[test]
    fn a_direct_message_is_named_after_who_is_in_it() {
        let dm = chat("90", vec![message("1", RIKER, "Poker?")]);
        assert_eq!(dm.display, "@Will Riker");
        assert_eq!(dm.named_after.len(), 1);
        assert!(dm.containers.is_empty());
        assert_eq!(
            dm.source_url.as_deref(),
            Some("https://discord.com/channels/@me/90")
        );
        let item = &dm.render_units[0].items[0];
        assert_eq!(
            item.source_url.as_deref(),
            Some("https://discord.com/channels/@me/90/1")
        );
    }

    /// A reply says what it answers, and is an edge to it; one whose
    /// message is not held is only its own words.
    #[test]
    fn a_reply_quotes_what_it_answers() {
        let mut reply = message("2", PICARD, "Make it so.");
        reply["type"] = json!(19);
        reply["message_reference"] = json!({"type": 0, "message_id": "1", "channel_id": "71"});
        let got = items(
            "71",
            vec![message("1", RIKER, "Shall we?\nNow?"), reply.clone()],
        );
        let body = got[1].text.as_deref().unwrap();
        assert!(
            body.starts_with("> ↪ [Will Riker](datalib:handle/discord/1702"),
            "{body}"
        );
        assert!(body.contains("Shall we?\n\nMake it so."), "{body}");
        assert_eq!(got[1].reply_to.as_ref(), Some(&got[0].message_uuid));
        let alone = items("71", vec![reply]);
        assert_eq!(alone[0].text.as_deref(), Some("Make it so."));
        assert_eq!(alone[0].reply_to, None);
    }

    /// Discord counts reactions and names nobody but the account.
    #[test]
    fn a_reaction_is_the_accounts_own_and_a_count() {
        let mut m = message("1", RIKER, "Promoted!");
        m["reactions"] = json!([
            {"emoji": {"id": null, "name": "🖖"}, "count": 4, "me": true},
            {"emoji": {"id": "55", "name": "borg"}, "count": 1, "me": false},
        ]);
        let got = items("71", vec![m]).remove(0).reactions;
        let shown: Vec<(&str, &str, u32)> = got
            .iter()
            .map(|r| (r.emoji.as_str(), r.reactor_display.as_str(), r.count))
            .collect();
        assert_eq!(
            shown,
            [
                ("🖖", "Jean-Luc Picard", 1),
                ("🖖", "3 others", 3),
                (":borg:", "1 person", 1)
            ]
        );
        assert_eq!(got[1].reactor_kind, AuthorKind::Unknown);
    }

    #[test]
    fn a_system_message_says_who_did_what() {
        let mut joined = message("1", RIKER, "");
        joined["type"] = json!(7);
        let mut thread = message("2", RIKER, "Warp core");
        thread["type"] = json!(18);
        let mut new = message("3", RIKER, "from the future");
        new["type"] = json!(99);
        let got = items("71", vec![joined, thread, new]);
        assert_eq!(got[0].kind, ItemKind::System);
        assert_eq!(
            got[0].system_note.as_deref(),
            Some("Will Riker joined the server")
        );
        assert_eq!(
            got[1].system_note.as_deref(),
            Some("Will Riker started a thread: Warp core")
        );
        assert_eq!(got[2].text.as_deref(), Some("from the future"));
        assert_eq!(got[2].problems.len(), 1, "a type nothing covers is said");
    }

    #[test]
    fn a_webhook_is_named_by_its_message() {
        let mut m = message("1", "55", "Deck 7 clear.");
        m["author"]["username"] = json!("Ship's computer");
        m["webhook_id"] = json!("55");
        let c = chat("71", vec![m]);
        let item = &c.render_units[0].items[0];
        assert_eq!(item.author_display, "Ship's computer");
        assert_eq!(item.author_handle, None);
        assert!(c.contacts.is_empty());
    }

    /// The account's Discord user is tied to its address; nobody else's is.
    #[test]
    fn the_accounts_own_contact_has_its_address() {
        let c = chat(
            "71",
            vec![message("1", RIKER, "Sir?"), message("2", PICARD, "Yes.")],
        );
        let handles: Vec<(bool, Vec<String>)> = c
            .contacts
            .iter()
            .map(|c| {
                let of = |h: &ContactHandle| h.handle.as_ref().unwrap().as_str().to_string();
                (c.account, c.handles.iter().map(of).collect())
            })
            .collect();
        assert_eq!(
            handles,
            [
                (false, vec!["discord:1702".to_string()]),
                (
                    true,
                    vec![
                        "discord:1701".to_string(),
                        "email:picard@enterprise.starfleet".to_string()
                    ]
                )
            ]
        );
    }

    #[test]
    fn a_file_is_its_blob_and_never_its_signed_link() {
        let mut m = message("1", RIKER, "");
        m["attachments"] = json!([{"id": "5", "filename": "warp.png", "size": 12,
            "content_type": "image/png", "url": "https://cdn.discordapp.com/x?ex=1"}]);
        let item = items("71", vec![m]).remove(0);
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(item.attachments[0].ref_id.as_deref(), Some("5"));
        assert_eq!(item.attachments[0].source_url, None);
    }
}
