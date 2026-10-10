//! Messenger: one conversation per chat, one document per year of it.
//! People are names here, and an account deleted since is `Facebook user`
//! or no name at all; `People` decides who each name is.

use std::collections::BTreeMap;

use datalib_etl_chat_common::period::by_year;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, NormalizedReaction, UpstreamRef,
};
use datalib_etl_facebook::ingest::schema_raw::{MESSENGER_MESSAGES_TABLE, MESSENGER_THREADS_TABLE};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use datalib_handle::Handle;
use serde_json::Value;

use datalib_schema::problems::{Problem, Reason, Severity};

use crate::common::{
    chat_item, media_attachment, noted, profile, shape_of, str_field, unread_keys,
};
use crate::ids;
use crate::processor::Owner;

pub fn messenger_profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::MESSENGER_STAMP_PRECISION,
        ..profile(
            "Facebook Conversation",
            "Facebook Message",
            ids::KIND_CONVERSATION,
            TextFormat::Markdown,
        )
    }
}

/// What a message carries its files under, beside `sticker`.
const MEDIA_LISTS: &[&str] = &["photos", "videos", "gifs", "audio_files", "files"];

/// Every message key this render reads, or leaves out on purpose (`ip`;
/// the two withheld flags are reported when set).
const MESSAGE_KEYS: &[&str] = &[
    "sender_name",
    "timestamp_ms",
    "content",
    "photos",
    "videos",
    "gifs",
    "audio_files",
    "files",
    "sticker",
    "share",
    "reactions",
    "is_unsent",
    "is_geoblocked_for_viewer",
    "is_unsent_image_by_messenger_kid_parent",
    "ip",
];

/// The conversation file's keys, beside the `messages` the ingest split
/// off.
const THREAD_KEYS: &[&str] = &[
    "participants",
    "title",
    "is_still_participant",
    "thread_path",
    "magic_words",
    "joinable_mode",
    "is_pending",
];
const SHARE_KEYS: &[&str] = &["link", "share_text"];
const REACTION_KEYS: &[&str] = &["reaction", "actor", "timestamp"];
const MEDIA_KEYS: &[&str] = &["uri", "creation_timestamp", "ai_stickers"];

/// How Facebook writes an account deleted since: `Facebook user`, or
/// nothing at all.
pub fn is_deleted_account(name: &str) -> bool {
    let name = name.trim();
    name.is_empty() || name.eq_ignore_ascii_case("Facebook user")
}

/// Who a name in one conversation is. A deleted account is told apart
/// only where the conversation lists just one: then the conversation's
/// id is the account's handle. Several in one conversation are
/// indistinguishable, and say so; one that left the conversation is not
/// listed, so how many there were is not known.
struct People<'a> {
    thread_id: &'a str,
    deleted: usize,
}

impl People<'_> {
    fn who(&self, name: &str) -> (String, Option<Handle>) {
        if !is_deleted_account(name) {
            let name = name.trim();
            return (name.to_string(), Handle::facebook_name(name));
        }
        match self.deleted {
            0 => ("Facebook user".to_string(), None),
            1 => (
                "Facebook user".to_string(),
                Handle::facebook_deleted(self.thread_id),
            ),
            n => (
                format!("Facebook user (one of {n} deleted accounts here)"),
                None,
            ),
        }
    }
}

/// Rows as `(row id, record)` from the two Messenger tables.
pub fn build_conversations(
    threads: &[(String, Value)],
    messages: &[(String, Value)],
    owner: &Owner,
) -> Vec<NormalizedChat> {
    let mut by_thread: BTreeMap<&str, Vec<(&str, &Value)>> = BTreeMap::new();
    for (id, m) in messages {
        if let Some(thread) = m.get("thread_id").and_then(Value::as_str) {
            by_thread.entry(thread).or_default().push((id, m));
        }
    }
    threads
        .iter()
        .map(|(row_id, t)| {
            let msgs = by_thread.remove(row_id.as_str()).unwrap_or_default();
            conversation(row_id, t, msgs, owner)
        })
        .collect()
}

fn conversation(
    row_id: &str,
    t: &Value,
    mut messages: Vec<(&str, &Value)>,
    owner: &Owner,
) -> NormalizedChat {
    let thread = t.get("thread").unwrap_or(&Value::Null);
    let participants: Vec<&str> = thread
        .get("participants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|p| p.get("name").and_then(Value::as_str).unwrap_or(""))
        .collect();
    let people = People {
        thread_id: row_id,
        deleted: participants
            .iter()
            .filter(|n| is_deleted_account(n))
            .count(),
    };
    let inputs = Inputs::default();
    inputs.read(MESSENGER_THREADS_TABLE, row_id);
    for input in &owner.inputs {
        inputs.read(&input.table, &input.id);
    }

    messages.sort_by_key(|(id, _)| order_key(id));
    let mut items: Vec<NormalizedChatItem> = messages
        .into_iter()
        .map(|(id, m)| {
            inputs.read(MESSENGER_MESSAGES_TABLE, id);
            let m = m.get("message").unwrap_or(&Value::Null);
            message(id, m, &people, owner, &inputs)
        })
        .collect();
    if let Some(first) = items.first_mut() {
        first
            .problems
            .extend(conversation_findings(thread, &participants, &people));
    }
    note_senders_not_listed(&mut items, &participants);

    let buckets = by_year(items)
        .into_iter()
        .map(|(period_key, items)| {
            let doc = ids::conversation_year(&owner.source_id, row_id, &period_key);
            NormalizedDoc {
                markdown_uuid: doc.uuid,
                source_ref: Some(UpstreamRef::new(doc.entity_kind, doc.natural_key)),
                items,
                period_key,
                orphan_reactions: Vec::new(),
            }
        })
        .collect();

    let chat = ids::conversation(&owner.source_id, row_id);
    NormalizedChat {
        contacts: Vec::new(),
        inputs: inputs.declared(),
        path_prefix: None,
        id: format!("conversation:{row_id}"),
        chat_uuid: chat.uuid,
        display: display(row_id, thread, &participants, owner),
        title: None,
        author: None,
        account: owner.account.clone(),
        project: Some(folder_label(str_field(t, "folder").unwrap_or("inbox"))),
        external_id: Some(chat.natural_key),
        source_url: None,
        upstream_account: None,
        org_uuid: None,
        org_name: None,
        buckets,
    }
}

/// What is true of a whole conversation, filed on its first message: a
/// key of the file this render does not read, deleted accounts that
/// cannot be told apart, a `magic_words` list with something in it.
fn conversation_findings(
    thread: &Value,
    participants: &[&str],
    people: &People<'_>,
) -> Vec<Problem> {
    let mut out = unread_keys(thread, THREAD_KEYS, "/thread");
    if people.deleted > 1 {
        out.push(noted(
            "participants",
            &format!(
                "{} deleted accounts among {} participants; their messages cannot be told apart",
                people.deleted,
                participants.len()
            ),
        ));
    }
    if let Some(words) = thread
        .get("magic_words")
        .filter(|w| w.as_array().is_some_and(|a| !a.is_empty()))
    {
        out.push(
            Problem::field("magic_words", Reason::UncoveredType, &shape_of(words))
                .at("/thread/magic_words")
                .severity(Severity::Warning),
        );
    }
    out
}

/// A sender the participants do not list — someone who left — noted once
/// per sender, on their first message.
fn note_senders_not_listed(items: &mut [NormalizedChatItem], participants: &[&str]) {
    let listed: Vec<&str> = participants.iter().map(|p| p.trim()).collect();
    let mut seen: Vec<String> = Vec::new();
    for item in items {
        let name = item.author_display.clone();
        if item.author_handle.is_none() || listed.contains(&name.as_str()) || seen.contains(&name) {
            continue;
        }
        if is_deleted_account(&name) {
            continue;
        }
        item.problems.push(noted(
            "sender_name",
            "a sender the conversation's participants do not list (left the conversation?)",
        ));
        seen.push(name);
    }
}

/// `<thread>:<timestamp_ms>:<n>` in time order, then `n`.
fn order_key(row_id: &str) -> (i64, usize) {
    let mut parts = row_id.rsplitn(3, ':');
    let n = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ms = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ms, n)
}

/// The conversation's title; else who is in it besides the owner. Every
/// deleted account's one-to-one conversation is titled `Facebook user`,
/// so each carries its id to be told apart in a list.
fn display(row_id: &str, thread: &Value, participants: &[&str], owner: &Owner) -> String {
    let title = str_field(thread, "title").unwrap_or("");
    if !title.is_empty() && !is_deleted_account(title) {
        return title.to_string();
    }
    let others: Vec<&str> = participants
        .iter()
        .map(|n| n.trim())
        .filter(|n| *n != owner.name && !is_deleted_account(n))
        .collect();
    if others.is_empty() {
        format!("Facebook user · {row_id}")
    } else {
        others.join(", ")
    }
}

fn folder_label(folder: &str) -> String {
    match folder {
        "inbox" => "Messenger",
        "filtered_threads" => "Messenger · filtered",
        "message_requests" => "Messenger · requests",
        "e2ee_cutover" => "Messenger · end-to-end encrypted",
        "archived_threads" => "Messenger · archived",
        other => return format!("Messenger · {other}"),
    }
    .to_string()
}

fn message(
    row_id: &str,
    m: &Value,
    people: &People<'_>,
    owner: &Owner,
    inputs: &Inputs,
) -> NormalizedChatItem {
    let mut problems = unread_keys(m, MESSAGE_KEYS, "/message");
    let date_ms = m
        .get("timestamp_ms")
        .and_then(Value::as_i64)
        .filter(|ms| *ms > 0);
    if date_ms.is_none() {
        let shape = m
            .get("timestamp_ms")
            .map_or("missing".to_string(), shape_of);
        problems.push(
            Problem::field("timestamp_ms", Reason::CoercionFailed, &shape)
                .at("/message/timestamp_ms"),
        );
    }
    for key in ["sender_name", "content"] {
        if let Some(v) = m.get(key).filter(|v| !v.is_string()) {
            problems.push(
                Problem::field(key, Reason::UncoveredType, &shape_of(v))
                    .at(format!("/message/{key}")),
            );
        }
    }
    for flag in [
        "is_geoblocked_for_viewer",
        "is_unsent_image_by_messenger_kid_parent",
    ] {
        if m.get(flag).and_then(Value::as_bool) == Some(true) {
            problems.push(
                noted(flag, "the export withholds this message's content")
                    .severity(Severity::Warning),
            );
        }
    }
    let (author, handle) = people.who(m.get("sender_name").and_then(Value::as_str).unwrap_or(""));

    let content = str_field(m, "content");
    let mut parts: Vec<String> = content.map(escape_md_block).into_iter().collect();
    if let Some(share) = m.get("share") {
        problems.extend(unread_keys(share, SHARE_KEYS, "/message/share"));
        let text = str_field(share, "share_text");
        let link = str_field(share, "link");
        match (text, link) {
            (Some(t), Some(l)) => {
                parts.push(format!("🔗 [{}]({})", escape_md_inline(t), md_link_dest(l)))
            }
            (None, Some(l)) if !content.is_some_and(|c| c.contains(l)) => {
                parts.push(format!("🔗 <{}>", md_link_dest(l)))
            }
            (Some(t), None) => parts.push(escape_md_block(t)),
            _ => {}
        }
    }

    let mut media: Vec<(String, &Value)> = Vec::new();
    for list in MEDIA_LISTS {
        let entries = m.get(*list).and_then(Value::as_array).into_iter().flatten();
        for (i, v) in entries.enumerate() {
            media.push((format!("/message/{list}/{i}"), v));
        }
    }
    if let Some(sticker) = m.get("sticker") {
        media.push(("/message/sticker".to_string(), sticker));
    }
    let mut attachments = Vec::new();
    for (path, v) in media {
        problems.extend(unread_keys(v, MEDIA_KEYS, &path));
        match media_attachment(v, row_id, inputs) {
            Some(att) => attachments.push(att),
            None => {
                problems.push(Problem::field("uri", Reason::UncoveredType, &shape_of(v)).at(path))
            }
        }
    }

    let unsent = m.get("is_unsent").and_then(Value::as_bool) == Some(true);
    if unsent && !(parts.is_empty() && attachments.is_empty()) {
        parts.push("*Unsent*".to_string());
    }
    let text = (!parts.is_empty()).then(|| parts.join("\n\n"));
    let empty = text.is_none() && attachments.is_empty();
    if empty && !unsent {
        problems.push(noted(
            "message",
            &format!("a message with nothing to show: {}", shape_of(m)),
        ));
    }
    let id = ids::message(&owner.source_id, row_id, date_ms);
    let mut item = chat_item(id, author, date_ms, text, attachments);
    item.author_handle = handle;
    // A message the export carries nothing of still happened: say so
    // rather than draw a bare header.
    if empty {
        item.kind = ItemKind::System;
        let note = if unsent {
            "Unsent"
        } else {
            "No content in the export"
        };
        item.system_note = Some(note.to_string());
    }
    let reactions = m.get("reactions").and_then(Value::as_array);
    for (i, r) in reactions.into_iter().flatten().enumerate() {
        let path = format!("/message/reactions/{i}");
        problems.extend(unread_keys(r, REACTION_KEYS, &path));
        match reaction(row_id, r, people, owner) {
            Some(reaction) => item.reactions.push(reaction),
            None => problems
                .push(Problem::field("reaction", Reason::UncoveredType, &shape_of(r)).at(path)),
        }
    }
    item.problems = problems;
    item
}

fn reaction(
    message_row_id: &str,
    r: &Value,
    people: &People<'_>,
    owner: &Owner,
) -> Option<NormalizedReaction> {
    let emoji = str_field(r, "reaction")?;
    let actor = r.get("actor").and_then(Value::as_str).unwrap_or("");
    let date_ms = r
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .map(|s| s * 1000);
    let id = ids::message_reaction(&owner.source_id, message_row_id, actor, emoji, date_ms);
    let (reactor_display, reactor_handle) = people.who(actor);
    Some(NormalizedReaction {
        reaction_uuid: id.uuid,
        reactor_handle,
        reactor_display,
        emoji: emoji.to_string(),
        date_ms,
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owner() -> Owner {
        Owner {
            source_id: "fb".to_string(),
            name: "Jean-Luc Picard".to_string(),
            account: None,
            inputs: Vec::new(),
        }
    }

    fn thread(id: &str, participants: &[&str], title: &str) -> (String, Value) {
        (
            id.to_string(),
            json!({"thread_id": id, "folder": "inbox", "thread": {
                "title": title,
                "participants": participants.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
            }}),
        )
    }

    fn msg(thread: &str, ms: i64, n: usize, mut m: Value) -> (String, Value) {
        m["timestamp_ms"] = json!(ms);
        (
            format!("{thread}:{ms:013}:{n}"),
            json!({"thread_id": thread, "message": m}),
        )
    }

    fn authors(chat: &NormalizedChat) -> Vec<(String, Option<String>)> {
        chat.buckets
            .iter()
            .flat_map(|b| &b.items)
            .map(|i| {
                (
                    i.author_display.clone(),
                    i.author_handle.as_ref().map(|h| h.to_string()),
                )
            })
            .collect()
    }

    /// One deleted account in a conversation is told apart by the
    /// conversation; two cannot be, and are not given a handle that
    /// would merge them.
    #[test]
    fn a_deleted_account_has_a_handle_only_where_it_is_the_only_one() {
        let one = build_conversations(
            &[thread(
                "2",
                &["Facebook user", "Jean-Luc Picard"],
                "Facebook user",
            )],
            &[
                msg(
                    "2",
                    1,
                    0,
                    json!({"sender_name": "Facebook user", "content": "hi"}),
                ),
                msg(
                    "2",
                    2,
                    0,
                    json!({"sender_name": "Jean-Luc Picard", "content": "who?"}),
                ),
            ],
            &owner(),
        );
        assert_eq!(one[0].display, "Facebook user · 2");
        assert_eq!(
            authors(&one[0]),
            [
                ("Facebook user".into(), Some("facebook:deleted/2".into())),
                (
                    "Jean-Luc Picard".into(),
                    Some("facebook:name/Jean-Luc Picard".into())
                ),
            ]
        );

        let group = build_conversations(
            &[thread(
                "4",
                &["Jean-Luc Picard", "Worf", "Facebook user", ""],
                "Ten Forward",
            )],
            &[
                msg("4", 1, 0, json!({"sender_name": "", "content": "a"})),
                msg(
                    "4",
                    2,
                    0,
                    json!({"sender_name": "Facebook user", "content": "b"}),
                ),
                msg("4", 3, 0, json!({"sender_name": "Worf", "content": "c"})),
            ],
            &owner(),
        );
        assert_eq!(group[0].display, "Ten Forward");
        let a = authors(&group[0]);
        assert_eq!(
            a[0],
            (
                "Facebook user (one of 2 deleted accounts here)".into(),
                None
            )
        );
        assert_eq!(a[1], a[0]);
        assert_eq!(a[2].1.as_deref(), Some("facebook:name/Worf"));
    }

    #[test]
    fn messages_read_oldest_first_and_split_by_year() {
        // 2369-01-01 and 2370-01-01, roughly, in ms.
        let y1 = 12_600_000_000_000;
        let y2 = y1 + 366 * 86_400_000;
        let chats = build_conversations(
            &[thread(
                "1",
                &["William Riker", "Jean-Luc Picard"],
                "William Riker",
            )],
            &[
                msg(
                    "1",
                    y2,
                    0,
                    json!({"sender_name": "William Riker", "content": "later"}),
                ),
                msg(
                    "1",
                    y1,
                    1,
                    json!({"sender_name": "Jean-Luc Picard", "content": "Now."}),
                ),
                msg(
                    "1",
                    y1,
                    0,
                    json!({"sender_name": "Jean-Luc Picard", "content": "Engage."}),
                ),
            ],
            &owner(),
        );
        let years: Vec<(&str, Vec<&str>)> = chats[0]
            .buckets
            .iter()
            .map(|b| {
                (
                    b.period_key.as_str(),
                    b.items.iter().filter_map(|i| i.text.as_deref()).collect(),
                )
            })
            .collect();
        assert_eq!(years.len(), 2, "{years:?}");
        assert_eq!(years[0].1, ["Engage.", "Now."]);
        assert_eq!(years[1].1, ["later"]);
    }

    #[test]
    fn a_share_unsent_message_and_reactions() {
        let chats = build_conversations(
            &[thread(
                "1",
                &["William Riker", "Jean-Luc Picard"],
                "William Riker",
            )],
            &[
                msg(
                    "1",
                    1,
                    0,
                    json!({"sender_name": "Jean-Luc Picard", "content": "Look",
                    "share": {"link": "https://risa.example/", "share_text": "Risa"}}),
                ),
                msg(
                    "1",
                    2,
                    0,
                    json!({"sender_name": "William Riker", "is_unsent": true}),
                ),
                msg(
                    "1",
                    3,
                    0,
                    json!({"sender_name": "William Riker", "content": "Tea?",
                    "reactions": [
                        {"reaction": "❤", "actor": "Jean-Luc Picard", "timestamp": 4},
                        {"reaction": "😆", "actor": "William Riker"},
                    ]}),
                ),
            ],
            &owner(),
        );
        let items = &chats[0].buckets[0].items;
        assert_eq!(
            items[0].text.as_deref(),
            Some("Look\n\n🔗 [Risa](https://risa.example/)")
        );
        assert_eq!(items[1].kind, ItemKind::System);
        assert_eq!(items[1].system_note.as_deref(), Some("Unsent"));
        let r = &items[2].reactions;
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].emoji, "❤");
        assert_eq!(r[0].date_ms, Some(4_000));
        assert_eq!(
            r[0].reactor_handle
                .as_ref()
                .map(|h| h.to_string())
                .as_deref(),
            Some("facebook:name/Jean-Luc Picard")
        );
        assert_eq!(r[1].date_ms, None);
        assert_ne!(r[0].reaction_uuid, r[1].reaction_uuid);
    }
}
