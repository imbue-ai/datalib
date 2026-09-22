//! The owner's own posts — timeline posts, check-ins, photo posts, life
//! events, and posts left on other people's pages — one chat-style
//! thread per post.

use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_facebook::ingest::schema_raw::{OTHER_POSTS_TABLE, POSTS_TABLE};

use crate::ids;
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::common::{
    attachment_entries, data_values, first_line, label_value, media_attachment, media_caption,
    profile, str_field, strip_mentions, truncate, ts_ms,
};
use crate::processor::Owner;

pub fn posts_profile() -> RenderProfile {
    profile("Facebook Post", "Facebook Post Message", ids::KIND_POST)
}

/// Rows as `(row id, record)` from the two post tables.
pub fn build_posts(
    posts: &[(String, Value)],
    other_posts: &[(String, Value)],
    owner: &Owner,
) -> Vec<NormalizedChat> {
    let mut chats: Vec<NormalizedChat> = posts
        .iter()
        .map(|(id, v)| timeline_post(id, v, owner))
        .collect();
    chats.extend(
        other_posts
            .iter()
            .map(|(id, v)| other_page_post(id, v, owner)),
    );
    chats
}

fn timeline_post(row_id: &str, v: &Value, owner: &Owner) -> NormalizedChat {
    let inputs = Inputs::default();
    inputs.read(POSTS_TABLE, row_id);
    let mut body: Vec<String> = data_values(v, "post")
        .filter_map(Value::as_str)
        .map(strip_mentions)
        .collect();
    let mut attachments = Vec::new();
    let mut places: Vec<String> = Vec::new();
    for entry in attachment_entries(v) {
        if let Some(media) = entry.get("media") {
            if let Some(att) = media_attachment(media, row_id, &inputs) {
                attachments.push(att);
                if let Some(caption) = media_caption(media, None) {
                    if !body.contains(&caption) {
                        body.push(caption);
                    }
                }
            }
        }
        if let Some(place) = entry.get("place") {
            place_line(place, &mut places);
        }
        if let Some(event) = entry.get("life_event") {
            let title = str_field(event, "title").unwrap_or("Life event");
            let mut s = format!("**{title}**");
            if let Some(d) = str_field(event, "description") {
                s.push_str("\n\n");
                s.push_str(&strip_mentions(d));
            }
            body.push(s);
            if let Some(place) = event.get("place") {
                place_line(place, &mut places);
            }
        }
        if let Some(ext) = entry.get("external_context") {
            let name = str_field(ext, "name");
            let url = str_field(ext, "url");
            match (name, url) {
                (Some(n), Some(u)) => body.push(format!("[{n}]({u})")),
                (None, Some(u)) => body.push(u.to_string()),
                (Some(n), None) => body.push(n.to_string()),
                (None, None) => {}
            }
        }
        if let Some(text) = str_field(entry, "text") {
            body.push(strip_mentions(text));
        }
    }
    body.extend(places);
    let tags: Vec<&str> = v
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| str_field(t, "name"))
        .collect();
    if !tags.is_empty() {
        body.push(format!("— with {}", tags.join(", ")));
    }

    let title = str_field(v, "title").map(str::to_string);
    let text = body.join("\n\n");
    let date_ms = ts_ms(v, "timestamp");
    let display = display_for(title.as_deref(), &text, "Facebook post");
    let item_id = ids::post_text(&owner.source_id, row_id, date_ms);
    one_item_chat(
        row_id,
        inputs,
        display,
        None,
        NormalizedChatItem {
            message_uuid: item_id.uuid,
            author_id: "me".to_string(),
            author_display: owner.name.clone(),
            date_ms,
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
            source_ref: Some(UpstreamRef::new(item_id.entity_kind, item_id.natural_key)),
            is_aside: false,
            problems: Vec::new(),
        },
        owner,
    )
}

/// A post on someone else's page or profile: the `label_values` shape,
/// keyed by Facebook's own `fbid`.
fn other_page_post(row_id: &str, v: &Value, owner: &Owner) -> NormalizedChat {
    let inputs = Inputs::default();
    inputs.read(OTHER_POSTS_TABLE, row_id);
    let mut body: Vec<String> = Vec::new();
    if let Some(msg) = label_value(v, "Message").and_then(|lv| str_field(lv, "value")) {
        body.push(strip_mentions(msg));
    }
    let mut attachments = Vec::new();
    for media in label_value(v, "Media")
        .and_then(|lv| lv.get("media"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(att) = media_attachment(media, row_id, &inputs) {
            attachments.push(att);
            if let Some(caption) = media_caption(media, None) {
                if !body.contains(&caption) {
                    body.push(caption);
                }
            }
        }
    }
    if let Some(feeling) = label_value(v, "Feeling/activity").and_then(|lv| str_field(lv, "value"))
    {
        body.push(format!("— {feeling}"));
    }
    let text = body.join("\n\n");
    let display = display_for(None, &text, "Facebook post on another page");
    let date_ms = ts_ms(v, "timestamp");
    let item_id = ids::post_text(&owner.source_id, row_id, date_ms);
    one_item_chat(
        row_id,
        inputs,
        display,
        None,
        NormalizedChatItem {
            message_uuid: item_id.uuid,
            author_id: "me".to_string(),
            author_display: owner.name.clone(),
            date_ms,
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
            source_ref: Some(UpstreamRef::new(item_id.entity_kind, item_id.natural_key)),
            is_aside: false,
            problems: Vec::new(),
        },
        owner,
    )
}

/// `📍 Name — address`, once per place: a check-in carries the same place
/// twice, with and without its page URL, and the one with the URL wins.
fn place_line(place: &Value, places: &mut Vec<String>) {
    let Some(name) = str_field(place, "name") else {
        return;
    };
    let mut line = match str_field(place, "url") {
        Some(url) => format!("📍 [{name}]({url})"),
        None => format!("📍 {name}"),
    };
    if let Some(addr) = str_field(place, "address") {
        line.push_str(&format!(" — {addr}"));
    }
    let same_place =
        |l: &String| l.contains(&format!("📍 {name}")) || l.contains(&format!("📍 [{name}]"));
    match places.iter().position(same_place) {
        Some(i) if line.len() > places[i].len() => places[i] = line,
        Some(_) => {}
        None => places.push(line),
    }
}

/// The post's title is Facebook's own summary ("X added 3 new photos.");
/// the first line of the body is what the person actually wrote.
fn display_for(title: Option<&str>, text: &str, fallback: &str) -> String {
    let snippet = first_line(text);
    if !snippet.is_empty() && !snippet.starts_with("**") && !snippet.starts_with("📍") {
        return truncate(snippet, 80);
    }
    title
        .map(|t| truncate(t, 80))
        .unwrap_or_else(|| fallback.to_string())
}

fn one_item_chat(
    row_id: &str,
    inputs: Inputs,
    display: String,
    source_url: Option<String>,
    item: NormalizedChatItem,
    owner: &Owner,
) -> NormalizedChat {
    for input in &owner.inputs {
        inputs.read(&input.table, &input.id);
    }
    let post = ids::post(&owner.source_id, row_id);
    NormalizedChat {
        inputs: inputs.declared(),
        path_prefix: None,
        id: format!("post:{row_id}"),
        chat_uuid: post.uuid.clone(),
        display,
        title: None,
        author: Some(owner.name.clone()),
        account: owner.account.clone(),
        project: None,
        external_id: Some(post.natural_key),
        source_url,
        upstream_scope: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: post.uuid,
            source_ref: None,
            items: vec![item],
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owner() -> Owner {
        Owner {
            source_id: "fb".to_string(),
            name: "Jean-Luc Picard".to_string(),
            account: Some("picard@enterprise.starfleet".to_string()),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn a_check_in_reads_as_text_plus_one_place_line() {
        let place = json!({"name": "Ten Forward", "address": "Deck 10"});
        let mut with_url = place.clone();
        with_url["url"] = json!("https://www.facebook.com/pages/Ten-Forward/1");
        let post = json!({
            "timestamp": 12_600_000_000_i64,
            "attachments": [{"data": [{"place": place}]}, {"data": [{"place": with_url}]}],
            "data": [{"post": "Tea, Earl Grey, hot."}, {}],
            "title": "Jean-Luc Picard was at Ten Forward.",
        });
        let chats = build_posts(&[("r1".to_string(), post)], &[], &owner());
        assert_eq!(chats.len(), 1);
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Text);
        assert_eq!(
            item.text.as_deref(),
            Some("Tea, Earl Grey, hot.\n\n📍 [Ten Forward](https://www.facebook.com/pages/Ten-Forward/1) — Deck 10")
        );
        assert_eq!(chats[0].display, "Tea, Earl Grey, hot.");
        assert_eq!(item.date_ms, Some(12_600_000_000_000));
        assert_eq!(chats[0].inputs.len(), 1);
        assert_eq!(chats[0].inputs[0].table, POSTS_TABLE);
    }

    #[test]
    fn photos_make_an_attachment_item_captioned_once() {
        let post = json!({
            "timestamp": 1,
            "attachments": [{"data": [
                {"media": {"uri": "your_facebook_activity/posts/media/a/1.jpg", "title": "Album"}},
                {"media": {"uri": "your_facebook_activity/posts/media/a/2.jpg", "title": "Album"}},
            ]}],
            "data": [{}],
            "title": "Jean-Luc Picard added 2 new photos.",
        });
        let chats = build_posts(&[("r1".to_string(), post)], &[], &owner());
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(item.attachments.len(), 2);
        assert_eq!(
            item.attachments[0].ref_id.as_deref(),
            Some("your_facebook_activity/posts/media/a/1.jpg")
        );
        assert_eq!(item.attachments[0].mime_type.as_deref(), Some("image/jpeg"));
        // Two photos titled "Album" contribute one caption, not two.
        assert_eq!(item.text.as_deref(), Some("Album"));
        assert_eq!(chats[0].display, "Album");
    }

    #[test]
    fn a_life_event_keeps_its_title_and_place_and_tags() {
        let post = json!({
            "timestamp": 1,
            "attachments": [{"data": [{"life_event": {
                "title": "Took command",
                "description": "A new ship. @[1:2048:Data]",
                "place": {"name": "Farpoint Station"},
            }}]}],
            "tags": [{"name": "Data"}],
            "data": [{"backdated_timestamp": 5}, {}],
            "title": "Jean-Luc Picard added a life event: Took command",
        });
        let chats = build_posts(&[("r1".to_string(), post)], &[], &owner());
        let text = chats[0].buckets[0].items[0].text.as_deref().unwrap();
        assert_eq!(
            text,
            "**Took command**\n\nA new ship. Data\n\n📍 Farpoint Station\n\n— with Data"
        );
        // A body that opens with the event heading is not a snippet; the
        // title names the thread.
        assert_eq!(
            chats[0].display,
            "Jean-Luc Picard added a life event: Took command"
        );
    }

    #[test]
    fn a_post_on_another_page_reads_the_label_values() {
        let post = json!({
            "timestamp": 7,
            "label_values": [
                {"label": "Message", "value": "Happy birthday, Number One."},
                {"label": "Media", "media": [{"uri": "your_facebook_activity/posts/media/p/1.jpg"}]},
                {"label": "Feeling/activity", "value": "feeling grateful"},
            ],
            "fbid": "400000000000001",
        });
        let chats = build_posts(&[], &[("400000000000001".to_string(), post)], &owner());
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(
            item.text.as_deref(),
            Some("Happy birthday, Number One.\n\n— feeling grateful")
        );
        assert_eq!(chats[0].external_id.as_deref(), Some("400000000000001"));
        let tables: Vec<&str> = chats[0].inputs.iter().map(|i| i.table.as_str()).collect();
        assert!(tables.contains(&OTHER_POSTS_TABLE), "{tables:?}");
        // The photo's edge is declared too, so the bytes arriving
        // re-renders the post.
        assert!(tables.contains(&"media_blobs"), "{tables:?}");
    }
}
