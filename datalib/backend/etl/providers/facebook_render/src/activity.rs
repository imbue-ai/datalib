//! The owner's comments and reactions. Neither names the post it was
//! left on in any way the export lets us resolve, so each feed is one
//! chat bucketed by month rather than a thread per post.

use std::collections::BTreeMap;

use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};
use datalib_etl_facebook::ingest::schema_raw::{ns_id, COMMENTS_TABLE, REACTIONS_TABLE};
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::common::{
    attachment_entries, data_values, label_value, media_attachment, month_of, profile, str_field,
    strip_mentions, ts_ms,
};
use crate::processor::Owner;

pub fn comments_profile() -> RenderProfile {
    profile("Facebook Comments", "Facebook Comment")
}

pub fn reactions_profile() -> RenderProfile {
    profile("Facebook Reactions", "Facebook Reaction")
}

pub const COMMENTS_CHAT: &str = "comments";
pub const REACTIONS_CHAT: &str = "reactions";

pub fn build_comments(comments: &[(String, Value)], owner: &Owner) -> Vec<NormalizedChat> {
    if comments.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let mut items = Vec::with_capacity(comments.len());
    for (row_id, v) in comments {
        inputs.read(COMMENTS_TABLE, row_id);
        let comment = data_values(v, "comment").next();
        let author = comment
            .and_then(|c| str_field(c, "author"))
            .unwrap_or(&owner.name)
            .to_string();
        let mut text = comment
            .and_then(|c| str_field(c, "comment"))
            .map(strip_mentions)
            .unwrap_or_default();
        if let Some(title) = str_field(v, "title") {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("*{title}*"));
        }
        let attachments: Vec<_> = attachment_entries(v)
            .filter_map(|e| e.get("media"))
            .filter_map(|m| media_attachment(m, row_id, &inputs))
            .collect();
        items.push(NormalizedChatItem {
            message_uuid: ns_id(&format!("msg:comment:{row_id}")),
            author_id: if author == owner.name {
                "me".to_string()
            } else {
                author.clone()
            },
            author_display: author,
            date_ms: ts_ms(v, "timestamp"),
            text: (!text.is_empty()).then_some(text),
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
        });
    }
    vec![monthly_chat(
        COMMENTS_CHAT,
        "Comments",
        items,
        inputs,
        owner,
    )]
}

/// The export ships reactions in two shapes, sometimes both for one
/// event: `label_values` rows (with the reaction, the URL and the
/// target's name) and `data[].reaction` rows (with a sentence of a
/// title). One item per `(timestamp, reaction)`, taking what each shape
/// has to offer.
pub fn build_reactions(reactions: &[(String, Value)], owner: &Owner) -> Vec<NormalizedChat> {
    if reactions.is_empty() {
        return Vec::new();
    }
    #[derive(Default)]
    struct Reaction {
        kind: String,
        url: Option<String>,
        target: Option<String>,
        title: Option<String>,
        row_ids: Vec<String>,
    }
    let inputs = Inputs::default();
    let mut by_key: BTreeMap<(i64, String), Reaction> = BTreeMap::new();
    for (row_id, v) in reactions {
        inputs.read(REACTIONS_TABLE, row_id);
        let Some(ms) = ts_ms(v, "timestamp") else {
            continue;
        };
        let from_labels = label_value(v, "Reaction").and_then(|lv| str_field(lv, "value"));
        let from_data = data_values(v, "reaction")
            .next()
            .and_then(|r| str_field(r, "reaction"));
        let kind = from_labels
            .or(from_data)
            .map(|k| k.to_lowercase())
            .unwrap_or_else(|| "reaction".to_string());
        let r = by_key.entry((ms, kind.clone())).or_default();
        r.kind = kind;
        r.row_ids.push(row_id.clone());
        if let Some(url) = label_value(v, "URL").and_then(|lv| str_field(lv, "value")) {
            r.url.get_or_insert_with(|| url.to_string());
        }
        if let Some(name) = target_name(v) {
            r.target.get_or_insert(name);
        }
        if let Some(title) = str_field(v, "title") {
            r.title.get_or_insert_with(|| title.to_string());
        }
    }

    let items = by_key
        .into_iter()
        .map(|((ms, _), r)| {
            let emoji = emoji_for(&r.kind);
            let what = match (&r.title, &r.target) {
                (Some(title), _) => title.clone(),
                (None, Some(target)) => format!("{}: {target}", capitalize(&r.kind)),
                (None, None) => capitalize(&r.kind),
            };
            // The URL rides on `source_url` alone: the message header
            // draws it as the `↗` link, so the body need not repeat it.
            let text = format!("{emoji} {what}");
            let key = r.row_ids.join("+");
            NormalizedChatItem {
                message_uuid: ns_id(&format!("msg:reaction:{key}")),
                author_id: "me".to_string(),
                author_display: owner.name.clone(),
                date_ms: Some(ms),
                text: Some(text),
                kind: ItemKind::Text,
                attachments: Vec::new(),
                reactions: Vec::new(),
                system_note: None,
                source_url: r.url.clone(),
                kind_label: None,
                source_ref: None,
                is_aside: false,
            }
        })
        .collect();
    vec![monthly_chat(
        REACTIONS_CHAT,
        "Reactions",
        items,
        inputs,
        owner,
    )]
}

/// The target's name: a bare `Name` label, or one nested under an
/// `Owner` dict, whichever the row carries.
fn target_name(v: &Value) -> Option<String> {
    if let Some(name) = label_value(v, "Name").and_then(|lv| str_field(lv, "value")) {
        return Some(name.to_string());
    }
    let owner = v
        .get("label_values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|lv| lv.get("title").and_then(Value::as_str) == Some("Owner"))?;
    fn find_name(v: &Value) -> Option<String> {
        match v {
            Value::Array(items) => items.iter().find_map(find_name),
            Value::Object(map) => {
                if map.get("label").and_then(Value::as_str) == Some("Name") {
                    return str_field(v, "value").map(str::to_string);
                }
                map.get("dict").and_then(find_name)
            }
            _ => None,
        }
    }
    find_name(owner)
}

fn emoji_for(kind: &str) -> &'static str {
    match kind {
        "like" => "👍",
        "love" => "❤️",
        "care" => "🤗",
        "haha" => "😆",
        "wow" => "😮",
        "sad" => "😢",
        "angry" | "anger" => "😡",
        _ => "👍",
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn monthly_chat(
    id: &str,
    display: &str,
    mut items: Vec<NormalizedChatItem>,
    inputs: Inputs,
    owner: &Owner,
) -> NormalizedChat {
    for input in &owner.inputs {
        inputs.read(&input.table, &input.id);
    }
    items.sort_by_key(|i| i.date_ms);
    let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
    for item in items {
        by_month
            .entry(month_of(item.date_ms))
            .or_default()
            .push(item);
    }
    NormalizedChat {
        inputs: inputs.declared(),
        path_prefix: None,
        id: id.to_string(),
        chat_uuid: ns_id(&format!("chat:{id}")),
        display: display.to_string(),
        title: None,
        author: Some(owner.name.clone()),
        account: owner.account.clone(),
        project: None,
        external_id: None,
        source_url: None,
        upstream_scope: None,
        org_uuid: None,
        org_name: None,
        buckets: by_month
            .into_iter()
            .map(|(period_key, items)| NormalizedDoc {
                orphan_reactions: Vec::new(),
                markdown_uuid: ns_id(&format!("doc:{id}:{period_key}")),
                period_key,
                items,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owner() -> Owner {
        Owner {
            name: "Jean-Luc Picard".to_string(),
            account: None,
            inputs: Vec::new(),
        }
    }

    // 2369-03 and 2369-04, in seconds.
    const MARCH: i64 = 12_598_000_000;
    const APRIL: i64 = 12_600_000_000;

    #[test]
    fn comments_bucket_by_month_and_keep_the_title() {
        let rows = vec![
            (
                "c1".to_string(),
                json!({
                    "timestamp": MARCH,
                    "data": [{"comment": {"timestamp": MARCH, "comment": "Enjoy the chair, @[1:2048:Will].", "author": "Jean-Luc Picard"}}],
                    "title": "Jean-Luc Picard commented on William Riker's post.",
                }),
            ),
            (
                "c2".to_string(),
                json!({
                    "timestamp": APRIL,
                    "attachments": [{"data": [{"media": {"uri": "m/4.png"}}]}],
                    "data": [{"comment": {"timestamp": APRIL, "comment": "Same view.", "author": "Jean-Luc Picard"}}],
                    "title": "Jean-Luc Picard commented on his own photo.",
                }),
            ),
        ];
        let chats = build_comments(&rows, &owner());
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].buckets.len(), 2, "one document per month");
        let first = &chats[0].buckets[0].items[0];
        assert_eq!(
            first.text.as_deref(),
            Some("Enjoy the chair, Will.\n\n*Jean-Luc Picard commented on William Riker's post.*")
        );
        assert_eq!(first.author_id, "me");
        let second = &chats[0].buckets[1].items[0];
        assert_eq!(second.kind, ItemKind::Attachment);
        assert_eq!(second.attachments[0].ref_id.as_deref(), Some("m/4.png"));
        // Two comment rows, plus the media edge the second one shows.
        assert_eq!(chats[0].inputs.len(), 3);
        assert!(chats[0]
            .inputs
            .iter()
            .any(|i| i.table == "media_blobs" && i.id == "c2#m/4.png"));
    }

    #[test]
    fn the_two_reaction_shapes_merge_into_one_item() {
        let rows = vec![
            (
                "500".to_string(),
                json!({"timestamp": MARCH, "media": [], "label_values": [
                    {"label": "Reaction", "value": "Like"},
                    {"label": "URL", "value": "https://www.facebook.com/will.riker/posts/1"},
                    {"label": "Name", "value": "William Riker"},
                ], "fbid": "500"}),
            ),
            (
                "hash".to_string(),
                json!({"timestamp": MARCH, "data": [{"reaction": {"reaction": "LIKE", "actor": "Jean-Luc Picard"}}],
                       "title": "Jean-Luc Picard liked William Riker's post."}),
            ),
            (
                "501".to_string(),
                json!({"timestamp": APRIL, "media": [], "label_values": [
                    {"label": "Reaction", "value": "Love"},
                    {"dict": [{"dict": [{"label": "Name", "value": "Guinan"}], "title": ""}], "title": "Owner"},
                ], "fbid": "501"}),
            ),
        ];
        let chats = build_reactions(&rows, &owner());
        assert_eq!(chats.len(), 1);
        let items: Vec<_> = chats[0].buckets.iter().flat_map(|b| &b.items).collect();
        assert_eq!(items.len(), 2, "two events, not three rows");
        assert_eq!(
            items[0].text.as_deref(),
            Some("👍 Jean-Luc Picard liked William Riker's post.")
        );
        assert_eq!(
            items[0].source_url.as_deref(),
            Some("https://www.facebook.com/will.riker/posts/1")
        );
        // No title in either shape: the reaction and the owner's name.
        assert_eq!(items[1].text.as_deref(), Some("❤️ Love: Guinan"));
        // Every row is declared, including the one that merged away.
        assert_eq!(chats[0].inputs.len(), 3);
    }
}
