//! The owner's comments and reactions. Neither names the post it was
//! left on in any way the export lets us resolve, so each feed is one
//! chat bucketed by year rather than a thread per post.

use std::collections::BTreeMap;

use datalib_etl_chat_common::period::year_of;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_facebook::ingest::schema_raw::{
    COMMENTS_TABLE, COMMENT_EDITS_TABLE, REACTIONS_TABLE,
};

use crate::edits::{histories, version_items, Target, Version};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_schema::problems::Problem;

use crate::ids;
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::common::{
    attachment_entries, chat_item, data_values, label_value, media_attachment, noted, profile,
    str_field, strip_mentions, ts_ms, unread_attachment_keys, unread_keys,
};
use crate::processor::Owner;

pub fn comments_profile() -> RenderProfile {
    profile(
        "Facebook Comments",
        "Facebook Comment",
        ids::KIND_FEED,
        TextFormat::Markdown,
    )
}

pub fn reactions_profile() -> RenderProfile {
    profile(
        "Facebook Reactions",
        "Facebook Reaction",
        ids::KIND_FEED,
        TextFormat::Plain,
    )
}

pub const COMMENTS_CHAT: &str = "comments";
pub const REACTIONS_CHAT: &str = "reactions";

/// Each comment's earlier versions fold in above it; an edit of a
/// comment the export no longer has stands at its own time.
pub fn build_comments(
    comments: &[(String, Value)],
    edits: &[(String, Value)],
    owner: &Owner,
) -> Vec<NormalizedChat> {
    if comments.is_empty() && edits.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let mut groups: Vec<Group> = Vec::with_capacity(comments.len());
    let mut texts: Vec<(String, Option<i64>)> = Vec::with_capacity(comments.len());
    for (row_id, v) in comments {
        inputs.read(COMMENTS_TABLE, row_id);
        let comment = data_values(v, "comment").next();
        let raw = comment.and_then(|c| str_field(c, "comment")).unwrap_or("");
        texts.push((raw.to_string(), ts_ms(v, "timestamp")));
        let author = comment
            .and_then(|c| str_field(c, "author"))
            .unwrap_or(&owner.name)
            .to_string();
        let mut text = comment
            .and_then(|c| str_field(c, "comment"))
            .map(|c| escape_md_block(&strip_mentions(c)))
            .unwrap_or_default();
        if let Some(title) = str_field(v, "title") {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("*{}*", escape_md_inline(title)));
        }
        let attachments: Vec<_> = attachment_entries(v)
            .filter_map(|e| e.get("media"))
            .filter_map(|m| media_attachment(m, row_id, &inputs))
            .collect();
        for url in attachment_entries(v)
            .filter_map(|e| e.get("external_context"))
            .filter_map(|c| str_field(c, "url"))
        {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("🔗 <{}>", md_link_dest(url)));
        }
        let date_ms = ts_ms(v, "timestamp");
        let item_id = ids::comment(&owner.source_id, row_id, date_ms);
        let mut item = chat_item(
            item_id,
            author,
            date_ms,
            (!text.is_empty()).then_some(text),
            attachments,
        );
        item.problems = comment_unread(v);
        groups.push(Group {
            at: date_ms,
            items: vec![item],
        });
    }

    for (row_id, _) in edits {
        inputs.read(COMMENT_EDITS_TABLE, row_id);
    }
    let targets: Vec<Target<'_>> = texts
        .iter()
        .map(|(text, date_ms)| Target {
            text,
            date_ms: *date_ms,
        })
        .collect();
    let versions = edits
        .iter()
        .filter_map(|(id, v)| Version::from_record(id, v))
        .collect();
    for history in histories(versions, &targets) {
        match history.target {
            Some(i) => {
                let earlier: Vec<&Version> = history.earlier(&texts[i].0).collect();
                let items =
                    version_items(&earlier, ids::KIND_COMMENT_VERSION, COMMENT_VERSION, owner);
                if let Some(item) = groups[i].items.last_mut() {
                    item.problems.extend(history.unshown_problems(&texts[i].0));
                }
                groups[i].items.splice(0..0, items);
            }
            None => {
                let all: Vec<&Version> = history.versions.iter().collect();
                let (shown, earlier) = all.split_last().expect("a history has a version");
                let mut items =
                    version_items(earlier, ids::KIND_COMMENT_VERSION, COMMENT_VERSION, owner);
                let mut last =
                    version_items(&[*shown], ids::KIND_COMMENT_VERSION, COMMENT_VERSION, owner);
                for item in &mut last {
                    item.branch.clear();
                    item.problems.push(noted(
                        "Text",
                        "an edit of a comment the export no longer has; rendered at its own time",
                    ));
                    if let Some(text) = &mut item.text {
                        text.push_str(
                            "\n\n*The last saved version of a comment the export no longer has.*",
                        );
                    }
                }
                items.extend(last);
                groups.push(Group {
                    at: shown.date_ms,
                    items,
                });
            }
        }
    }
    vec![yearly_chat(
        COMMENTS_CHAT,
        "Comments",
        groups,
        inputs,
        owner,
    )]
}

const COMMENT_VERSION: &str = "Facebook Comment Version";

/// Items that read together, filed by one time: a comment and the
/// versions folded in above it go into the year the comment was made.
struct Group {
    at: Option<i64>,
    items: Vec<NormalizedChatItem>,
}

/// What render does not read of a comment record.
fn comment_unread(v: &Value) -> Vec<Problem> {
    let mut out = unread_keys(v, &["timestamp", "data", "title", "attachments"], "");
    let data = v.get("data").and_then(Value::as_array);
    for (i, d) in data.into_iter().flatten().enumerate() {
        let path = format!("/data/{i}");
        out.extend(unread_keys(d, &["comment"], &path));
        if let Some(c) = d.get("comment") {
            out.extend(unread_keys(
                c,
                &["author", "comment", "timestamp"],
                &format!("{path}/comment"),
            ));
        }
    }
    out.extend(unread_attachment_keys(v));
    out
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

    let items: Vec<NormalizedChatItem> = by_key
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
            let row_ids: Vec<&str> = r.row_ids.iter().map(String::as_str).collect();
            let item_id = ids::reaction(&owner.source_id, &row_ids, Some(ms));
            NormalizedChatItem {
                source_url: r.url.clone(),
                ..chat_item(
                    item_id,
                    owner.name.clone(),
                    Some(ms),
                    Some(text),
                    Vec::new(),
                )
            }
        })
        .collect();
    let groups = items
        .into_iter()
        .map(|item| Group {
            at: item.date_ms,
            items: vec![item],
        })
        .collect();
    vec![yearly_chat(
        REACTIONS_CHAT,
        "Reactions",
        groups,
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

fn yearly_chat(
    id: &str,
    display: &str,
    mut groups: Vec<Group>,
    inputs: Inputs,
    owner: &Owner,
) -> NormalizedChat {
    groups.sort_by_key(|g| g.at);
    let mut years: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
    for g in groups {
        years.entry(year_of(g.at)).or_default().extend(g.items);
    }
    for input in &owner.inputs {
        inputs.read(&input.table, &input.id);
    }
    let feed = ids::feed(&owner.source_id, id);
    NormalizedChat {
        contacts: Vec::new(),
        inputs: inputs.declared(),
        path_prefix: None,
        id: id.to_string(),
        chat_uuid: feed.uuid,
        display: display.to_string(),
        title: None,
        author: Some(owner.name.clone()),
        account: owner.account.clone(),
        project: None,
        external_id: Some(feed.natural_key),
        source_url: None,
        upstream_account: None,
        org_uuid: None,
        org_name: None,
        buckets: years
            .into_iter()
            .map(|(period_key, items)| {
                let year = ids::feed_year(&owner.source_id, id, &period_key);
                NormalizedDoc {
                    orphan_reactions: Vec::new(),
                    markdown_uuid: year.uuid,
                    source_ref: Some(UpstreamRef::new(year.entity_kind, year.natural_key)),
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
    use datalib_etl_chat_common::types::ItemKind;
    use serde_json::json;

    fn owner() -> Owner {
        Owner {
            source_id: "fb".to_string(),
            name: "Jean-Luc Picard".to_string(),
            account: None,
            inputs: Vec::new(),
        }
    }

    // 2369-03, 2369-04 and 2370-03, in seconds.
    const MARCH: i64 = 12_598_000_000;
    const APRIL: i64 = 12_600_000_000;
    const NEXT_YEAR: i64 = MARCH + 365 * 86_400;

    #[test]
    fn comments_bucket_by_year_and_keep_the_title() {
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
                    "timestamp": NEXT_YEAR,
                    "attachments": [{"data": [{"media": {"uri": "m/4.png"}}]}],
                    "data": [{"comment": {"timestamp": NEXT_YEAR, "comment": "Same view.", "author": "Jean-Luc Picard"}}],
                    "title": "Jean-Luc Picard commented on his own photo.",
                }),
            ),
        ];
        let chats = build_comments(&rows, &[], &owner());
        assert_eq!(chats.len(), 1);
        let years: Vec<&str> = chats[0]
            .buckets
            .iter()
            .map(|b| b.period_key.as_str())
            .collect();
        assert_eq!(years, ["2369", "2370"], "one document per year");
        let first = &chats[0].buckets[0].items[0];
        assert_eq!(
            first.text.as_deref(),
            Some("Enjoy the chair, Will.\n\n*Jean-Luc Picard commented on William Riker's post.*")
        );
        assert_eq!(first.author_display, owner().name);
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
