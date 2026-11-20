//! What the activity feeds share. Gemini, YouTube and Google Maps are
//! each a timeline of things the account did, so each is one chat datalib
//! composes, one document per feed.

use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, UpstreamRef,
};
use datalib_etl_render::inputs::Inputs;
use datalib_id::Identity;
use datalib_schema::problems::Problem;
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::ids;

pub const GEMINI: &str = "feed:gemini";
pub const YOUTUBE_HISTORY: &str = "feed:youtube_history";
pub const YOUTUBE_SUBSCRIPTIONS: &str = "feed:youtube_subscriptions";
pub const MAPS: &str = "feed:maps";
/// One page per Saved list, My Maps map and Play Books book: the prefix,
/// then the export file it comes from.
pub const SAVED_LIST: &str = "feed:saved_list:";
pub const MY_MAP: &str = "feed:my_map:";
pub const PLAY_BOOK: &str = "feed:play_book:";

/// Who every feed's own items are by: the account the export is of.
pub const ME: &str = "Me";

/// One raw row of a feed table: its id, its payload and, for a table
/// that has one, its `when_ts`.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub payload: Value,
    pub when: Option<String>,
}

pub fn is_feed(chat_id: &str) -> bool {
    chat_id.starts_with("feed:")
}

pub fn profile(chat_id: &str) -> RenderProfile {
    let (source_label, chat_kind, message_kind) = match chat_id {
        GEMINI => ("Gemini", "Gemini Activity", "Gemini Prompt"),
        YOUTUBE_HISTORY => ("YouTube", "YouTube History", "YouTube Watch"),
        YOUTUBE_SUBSCRIPTIONS => ("YouTube", "YouTube Subscriptions", "YouTube Subscription"),
        id if id.starts_with(SAVED_LIST) => {
            ("Google Maps", "Google Maps List", "Google Maps Saved Item")
        }
        id if id.starts_with(MY_MAP) => ("My Maps", "My Maps Map", "My Maps Placemark"),
        id if id.starts_with(PLAY_BOOK) => ("Play Books", "Play Books Book", "Play Books Entry"),
        _ => ("Google Maps", "Google Maps Activity", "Google Maps Place"),
    };
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::GoogleTakeout,
        source_label: source_label.to_string(),
        chat_kind: chat_kind.to_string(),
        message_kind: message_kind.to_string(),
        reaction_kind: format!("{source_label} Reaction"),
        chat_entity_kind: ids::KIND_FEED,
        render_version: crate::render::RENDER_VERSION,
        // Each feed builds its markdown itself, escaping what it quotes.
        text_format: TextFormat::Markdown,
        layout: datalib_etl_chat_common::Layout::Conversation,
    }
}

/// An item with no text yet; `kind_label` is what the grid calls it.
pub fn item(
    id: Identity,
    author: &str,
    date_ms: Option<i64>,
    kind_label: &str,
    problems: Vec<Problem>,
) -> NormalizedChatItem {
    NormalizedChatItem {
        reply_to: None,
        message_uuid: id.uuid,
        author_handle: None,
        author_kind: AuthorKind::Named,
        author_display: author.to_string(),
        date_ms,
        date_offset_secs: None,
        text: None,
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        replies: None,
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems,
    }
}

/// An item of the account's own, under the "Me" that stands in for it.
pub fn own_item(
    id: Identity,
    date_ms: Option<i64>,
    kind_label: &str,
    problems: Vec<Problem>,
) -> NormalizedChatItem {
    NormalizedChatItem {
        author_kind: AuthorKind::Account,
        ..item(id, ME, date_ms, kind_label, problems)
    }
}

/// A line at the top of a page saying what it is — a list's description,
/// a book's author — as a system note.
pub fn note_item(source_id: &str, chat_id: &str, note: String) -> NormalizedChatItem {
    let id = ids::feed_item(source_id, ids::KIND_FEED_NOTE, chat_id, None);
    NormalizedChatItem {
        kind: ItemKind::System,
        system_note: Some(note),
        ..item(id, ME, None, "Note", Vec::new())
    }
}

/// What the account wrote about a whole page — a list's or a map's
/// description — as its first entry, so it is searched with the rest.
pub fn description_item(source_id: &str, chat_id: &str, text: String) -> NormalizedChatItem {
    let id = ids::feed_item(source_id, ids::KIND_FEED_NOTE, chat_id, None);
    with_text(own_item(id, None, "Description", Vec::new()), Some(text))
}

/// HTML someone wrote (a pin's description, Gemini's answer) as
/// markdown. Pictures are left to the attachments that carry them.
pub fn html_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "img"])
        .build()
        .convert(html)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn with_text(mut item: NormalizedChatItem, text: Option<String>) -> NormalizedChatItem {
    item.text = text;
    item
}

pub fn with_attachments(
    mut item: NormalizedChatItem,
    attachments: Vec<NormalizedAttachment>,
) -> NormalizedChatItem {
    if !attachments.is_empty() {
        item.kind = ItemKind::Attachment;
    }
    item.attachments = attachments;
    item
}

/// An attachment whose bytes the feed's bundle holds under `ref_id`,
/// shown as `name`; its type is read off `file`, the name in the export.
pub fn attachment(ref_id: String, name: &str, file: &str) -> NormalizedAttachment {
    NormalizedAttachment {
        href: None,
        file_name: Some(name.to_string()),
        mime_type: crate::render::mime_of(file),
        byte_len: None,
        source_url: None,
        ref_id: Some(ref_id),
    }
}

/// A row's `when_ts` as unix millis. The ingest keeps what the export
/// wrote: RFC 3339, or — for a Maps photo — unix seconds. A date the
/// ingest could not read is stored as no `when_ts` beside the export's own
/// words (`whenStr`), which are offered instead, so the loss is reported.
pub fn stamp_ms(row: &Row, problems: &mut Vec<Problem>) -> Option<i64> {
    own_stamp_ms(
        row.when
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| str_at(&row.payload, "/whenStr")),
        "when_ts",
        |s| match s.parse::<i64>() {
            Ok(seconds) => Some(seconds * 1000),
            Err(_) => datalib_time::parse_strict(s)
                .ok()
                .map(|t| t.to_unix_millis()),
        },
        problems,
    )
}

pub fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A timeline feed as one chat, oldest first; the undated last, in the
/// order given.
pub fn timeline(
    source_id: &str,
    chat_id: &str,
    display: &str,
    mut items: Vec<NormalizedChatItem>,
    inputs: Inputs,
) -> NormalizedChat {
    items.sort_by_key(|i| (i.date_ms.is_none(), i.date_ms));
    feed_chat(source_id, chat_id, display, items, inputs)
}

/// The feed `chat_id` as one chat in one document, in the order given.
pub fn feed_chat(
    source_id: &str,
    chat_id: &str,
    display: &str,
    items: Vec<NormalizedChatItem>,
    inputs: Inputs,
) -> NormalizedChat {
    let feed = ids::feed(source_id, chat_id);
    NormalizedChat {
        named_after: Vec::new(),
        contacts: Vec::new(),
        inputs: inputs.declared(),
        id: chat_id.to_string(),
        containers: Vec::new(),
        filed_under: Vec::new(),
        chat_uuid: feed.uuid.clone(),
        display: display.to_string(),
        title: None,
        author: Some(ME.to_string()),
        account: None,
        project: Some(profile(chat_id).source_label),
        external_id: Some(feed.natural_key),
        source_url: None,
        upstream_account: None,
        org_uuid: None,
        org_name: None,
        render_units: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            markdown_uuid: feed.uuid,
            source_ref: None,
            items,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(when: &str) -> Row {
        Row {
            id: "r".to_string(),
            payload: Value::Null,
            when: Some(when.to_string()),
        }
    }

    #[test]
    fn a_stamp_reads_as_rfc_3339_or_unix_seconds() {
        let mut problems = Vec::new();
        assert_eq!(
            stamp_ms(&row("2364-03-01T09:00:00Z"), &mut problems),
            Some(12_438_608_400_000)
        );
        assert_eq!(
            stamp_ms(&row("12446854200"), &mut problems),
            Some(12_446_854_200_000)
        );
        assert!(problems.is_empty());
        assert_eq!(stamp_ms(&row("stardate 41153.7"), &mut problems), None);
        assert_eq!(problems.len(), 1, "a stamp that will not read is said");
    }

    /// A YouTube watch whose date the ingest could not read was stored with
    /// no `when_ts`, and rendered undated with nothing said.
    #[test]
    fn a_date_the_ingest_could_not_read_is_said() {
        let unread = Row {
            id: "r".to_string(),
            payload: serde_json::json!({"whenStr": "Mar 1, 2364, 9:00:00 AM IST"}),
            when: None,
        };
        let mut problems = Vec::new();
        assert_eq!(stamp_ms(&unread, &mut problems), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].sample.contains("IST"), "{problems:?}");

        let undated = Row {
            id: "r".to_string(),
            payload: serde_json::json!({"channelTitle": "Starfleet Academy"}),
            when: None,
        };
        let mut problems = Vec::new();
        assert_eq!(stamp_ms(&undated, &mut problems), None);
        assert!(problems.is_empty(), "a row with no date has nothing to say");
    }
}
