//! What the activity feeds share. Gemini, YouTube and Google Maps are
//! each a timeline of things the account did, so each is one chat datalib
//! composes, a document per year; YouTube's subscriptions have no dates
//! and are one document.

use std::collections::BTreeMap;

use datalib_etl_chat_common::period::by_year;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    own_stamp_ms, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
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
        message_uuid: id.uuid,
        author_handle: None,
        author_display: author.to_string(),
        date_ms,
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
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems,
    }
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
        rel_path: None,
        file_name: Some(name.to_string()),
        mime_type: crate::render::mime_of(file),
        byte_len: None,
        source_url: None,
        ref_id: Some(ref_id),
    }
}

/// A row's `when_ts` as unix millis. The ingest keeps what the export
/// wrote: RFC 3339, or — for a Maps photo — unix seconds.
pub fn stamp_ms(row: &Row, problems: &mut Vec<Problem>) -> Option<i64> {
    own_stamp_ms(
        row.when.as_deref().filter(|s| !s.is_empty()),
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

/// The feed `chat_id` as one chat, a document per year.
pub fn yearly(
    source_id: &str,
    chat_id: &str,
    display: &str,
    items: Vec<NormalizedChatItem>,
    inputs: Inputs,
) -> NormalizedChat {
    chat(source_id, chat_id, display, by_year(items), inputs)
}

/// The feed `chat_id` as one chat in one document.
pub fn whole(
    source_id: &str,
    chat_id: &str,
    display: &str,
    items: Vec<NormalizedChatItem>,
    inputs: Inputs,
) -> NormalizedChat {
    chat(
        source_id,
        chat_id,
        display,
        BTreeMap::from([("all".to_string(), items)]),
        inputs,
    )
}

fn chat(
    source_id: &str,
    chat_id: &str,
    display: &str,
    periods: BTreeMap<String, Vec<NormalizedChatItem>>,
    inputs: Inputs,
) -> NormalizedChat {
    let feed = ids::feed(source_id, chat_id);
    NormalizedChat {
        contacts: Vec::new(),
        inputs: inputs.declared(),
        path_prefix: None,
        id: chat_id.to_string(),
        chat_uuid: feed.uuid,
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
        buckets: periods
            .into_iter()
            .map(|(period_key, items)| {
                let period = ids::feed_period(source_id, chat_id, &period_key);
                NormalizedDoc {
                    orphan_reactions: Vec::new(),
                    markdown_uuid: period.uuid,
                    source_ref: Some(UpstreamRef::new(period.entity_kind, period.natural_key)),
                    period_key,
                    items,
                }
            })
            .collect(),
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
}
