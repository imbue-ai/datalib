//! The owner's own posts — timeline posts, check-ins, photo posts, life
//! events, and posts left on other people's pages — one chat-style
//! thread per post.

use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{NormalizedChat, NormalizedChatItem, NormalizedDoc};
use datalib_etl_facebook::ingest::schema_raw::{OTHER_POSTS_TABLE, POSTS_TABLE, POST_EDITS_TABLE};
use datalib_etl_render::inputs::Input;

use crate::edits::{histories, version_items, Target, Version};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};

use crate::ids;
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::common::{
    attachment_entries, chat_item, data_values, first_line, label_value, media_attachment,
    media_caption, noted, profile, str_field, strip_mentions, truncate, ts_ms,
    unread_attachment_keys, unread_keys, unread_labels,
};
use crate::processor::Owner;

pub fn posts_profile() -> RenderProfile {
    profile(
        "Facebook Post",
        "Facebook Post Message",
        ids::KIND_POST,
        TextFormat::Markdown,
    )
}

/// A post's body as it is built: what the person wrote, escaped for the
/// page, and the markdown this file adds around it. The words as written
/// are kept too, for the thread's title and to spot a repeated caption.
#[derive(Default)]
struct Body {
    parts: Vec<(String, Option<String>)>,
}

impl Body {
    fn typed(&mut self, text: String) {
        self.parts.push((escape_md_block(&text), Some(text)));
    }

    /// Markdown built here, with every plain string in it escaped.
    fn markup(&mut self, md: String) {
        self.parts.push((md, None));
    }

    fn has_typed(&self, text: &str) -> bool {
        self.parts.iter().any(|(_, t)| t.as_deref() == Some(text))
    }

    /// What the post opens with, when that is something the person wrote.
    fn opening_words(&self) -> Option<&str> {
        self.parts.first().and_then(|(_, t)| t.as_deref())
    }

    fn markdown(&self) -> String {
        self.parts
            .iter()
            .map(|(md, _)| md.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Rows as `(row id, record)` from the two post tables and the edits
/// file. Each post's earlier versions fold in above its text; an edit of
/// a post the export no longer has is a post of its own.
pub fn build_posts(
    posts: &[(String, Value)],
    other_posts: &[(String, Value)],
    edits: &[(String, Value)],
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

    let texts: Vec<(String, Option<i64>)> = posts
        .iter()
        .map(|(_, v)| (timeline_text(v), ts_ms(v, "timestamp")))
        .chain(other_posts.iter().map(|(_, v)| {
            let text = label_value(v, "Message").and_then(|lv| str_field(lv, "value"));
            (text.unwrap_or("").to_string(), ts_ms(v, "timestamp"))
        }))
        .collect();
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
    // Which post an edit is of is decided over every edit and every post:
    // a new edit may be any post's, and a post deleted or changed moves
    // its versions to another post or to a document of their own. So
    // where there are edits, every post, and every edit of a post gone,
    // reads all of them, and the processor renders every post again when
    // any of them changes (`narrow_chats`).
    let edit_inputs: Vec<Input> = if edits.is_empty() {
        Vec::new()
    } else {
        let all = |table: &'static str, rows: &[(String, Value)]| {
            rows.iter()
                .map(|(id, _)| Input::new(table, id))
                .collect::<Vec<_>>()
        };
        [
            all(POST_EDITS_TABLE, edits),
            all(POSTS_TABLE, posts),
            all(OTHER_POSTS_TABLE, other_posts),
        ]
        .concat()
    };
    for chat in &mut chats {
        chat.inputs.extend(edit_inputs.iter().cloned());
    }
    for history in histories(versions, &targets) {
        match history.target {
            Some(i) => {
                let earlier: Vec<&Version> = history.earlier(&texts[i].0).collect();
                let items = version_items(&earlier, ids::KIND_POST_VERSION, POST_VERSION, owner);
                let post = &mut chats[i].buckets[0].items;
                if let Some(item) = post.last_mut() {
                    item.problems.extend(history.unshown_problems(&texts[i].0));
                }
                post.splice(0..0, items);
            }
            None => chats.push(edited_post_not_in_export(
                &history.versions,
                &edit_inputs,
                owner,
            )),
        }
    }
    chats
}

const POST_VERSION: &str = "Facebook Post Version";

fn timeline_text(v: &Value) -> String {
    data_values(v, "post")
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The versions of a post the export does not have: its last version
/// shown, the earlier ones folded in above it.
fn edited_post_not_in_export(
    versions: &[Version],
    edit_inputs: &[Input],
    owner: &Owner,
) -> NormalizedChat {
    let (shown, earlier) = versions.split_last().expect("a history has a version");
    let inputs = Inputs::default();
    for input in edit_inputs {
        inputs.read(&input.table, &input.id);
    }
    let text = format!(
        "{}\n\n*The last saved version of a post the export no longer has.*",
        escape_md_block(&strip_mentions(&shown.text))
    );
    let id = ids::version(
        &owner.source_id,
        ids::KIND_POST_VERSION,
        &shown.row_id,
        shown.date_ms,
    );
    let mut problems = shown.problems.clone();
    problems.push(noted(
        "Text",
        "an edit of a post the export no longer has; rendered as a post of its own",
    ));
    let item = NormalizedChatItem {
        kind_label: Some(POST_VERSION.to_string()),
        problems,
        ..chat_item(
            id,
            owner.name.clone(),
            shown.date_ms,
            Some(text),
            Vec::new(),
        )
    };
    let display = display_for(None, Some(&shown.text), "Edited Facebook post");
    let mut chat = one_item_chat(&shown.row_id, inputs, display, None, item, owner);
    let earlier: Vec<&Version> = earlier.iter().collect();
    let items = version_items(&earlier, ids::KIND_POST_VERSION, POST_VERSION, owner);
    chat.buckets[0].items.splice(0..0, items);
    chat
}

fn timeline_post(row_id: &str, v: &Value, owner: &Owner) -> NormalizedChat {
    let inputs = Inputs::default();
    inputs.read(POSTS_TABLE, row_id);
    let mut body = Body::default();
    for post in data_values(v, "post").filter_map(Value::as_str) {
        body.typed(strip_mentions(post));
    }
    let mut attachments = Vec::new();
    let mut places: Vec<String> = Vec::new();
    for entry in attachment_entries(v) {
        if let Some(media) = entry.get("media") {
            if let Some(att) = media_attachment(media, row_id, &inputs) {
                attachments.push(att);
                if let Some(caption) = media_caption(media, None) {
                    if !body.has_typed(&caption) {
                        body.typed(caption);
                    }
                }
            }
        }
        if let Some(place) = entry.get("place") {
            place_line(place, &mut places);
        }
        if let Some(event) = entry.get("life_event") {
            let title = str_field(event, "title").unwrap_or("Life event");
            let mut s = format!("**{}**", escape_md_inline(title));
            if let Some(d) = str_field(event, "description") {
                s.push_str("\n\n");
                s.push_str(&escape_md_block(&strip_mentions(d)));
            }
            body.markup(s);
            if let Some(place) = event.get("place") {
                place_line(place, &mut places);
            }
        }
        if let Some(ext) = entry.get("external_context") {
            let name = str_field(ext, "name");
            let url = str_field(ext, "url");
            match (name, url) {
                (Some(n), Some(u)) => {
                    body.markup(format!("[{}]({})", escape_md_inline(n), md_link_dest(u)))
                }
                (None, Some(u)) => body.typed(u.to_string()),
                (Some(n), None) => body.typed(n.to_string()),
                (None, None) => {}
            }
        }
        if let Some(text) = str_field(entry, "text") {
            body.typed(strip_mentions(text));
        }
    }
    for place in places {
        body.markup(place);
    }
    let tags: Vec<String> = v
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| str_field(t, "name"))
        .map(escape_md_inline)
        .collect();
    if !tags.is_empty() {
        body.markup(format!("— with {}", tags.join(", ")));
    }

    let title = str_field(v, "title").map(str::to_string);
    let text = body.markdown();
    let date_ms = ts_ms(v, "timestamp");
    let display = display_for(title.as_deref(), body.opening_words(), "Facebook post");
    let item_id = ids::post_text(&owner.source_id, row_id, date_ms);
    let mut item = chat_item(
        item_id,
        owner.name.clone(),
        date_ms,
        (!text.is_empty()).then_some(text),
        attachments,
    );
    item.problems = timeline_post_unread(v);
    one_item_chat(row_id, inputs, display, None, item, owner)
}

/// What render does not read of a timeline post. `update_timestamp` is
/// read and left out: on a real export it is the post's own time on all
/// but one post in sixty-three, so it says nothing about an edit.
fn timeline_post_unread(v: &Value) -> Vec<datalib_schema::problems::Problem> {
    let mut out = unread_keys(
        v,
        &["timestamp", "attachments", "data", "title", "tags"],
        "",
    );
    let data = v.get("data").and_then(Value::as_array);
    for (i, d) in data.into_iter().flatten().enumerate() {
        out.extend(unread_keys(
            d,
            &["post", "update_timestamp", "backdated_timestamp"],
            &format!("/data/{i}"),
        ));
    }
    let tags = v.get("tags").and_then(Value::as_array);
    for (i, t) in tags.into_iter().flatten().enumerate() {
        out.extend(unread_keys(t, &["name"], &format!("/tags/{i}")));
    }
    out.extend(unread_attachment_keys(v));
    out
}

/// The labels of a post on someone else's page that render reads, or
/// leaves out on purpose: which app wrote it, the language Facebook
/// guessed, whether to translate it.
const OTHER_PAGE_LABELS: &[&str] = &[
    "Message",
    "Media",
    "Feeling/activity",
    "Last modified",
    "Detected dialect",
    "App used at creation time",
    "Third-party app used at creation time",
    "Translation should be skipped",
];

/// A post on someone else's page or profile: the `label_values` shape,
/// keyed by Facebook's own `fbid`.
fn other_page_post(row_id: &str, v: &Value, owner: &Owner) -> NormalizedChat {
    let inputs = Inputs::default();
    inputs.read(OTHER_POSTS_TABLE, row_id);
    let mut body = Body::default();
    if let Some(msg) = label_value(v, "Message").and_then(|lv| str_field(lv, "value")) {
        body.typed(strip_mentions(msg));
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
                if !body.has_typed(&caption) {
                    body.typed(caption);
                }
            }
        }
    }
    if let Some(feeling) = label_value(v, "Feeling/activity").and_then(|lv| str_field(lv, "value"))
    {
        body.markup(format!("— {}", escape_md_inline(feeling)));
    }
    let date_ms = ts_ms(v, "timestamp");
    if let Some(edited) = label_value(v, "Last modified")
        .and_then(|lv| lv.get("timestamp_value"))
        .and_then(Value::as_i64)
        .filter(|s| *s > 0 && Some(*s * 1000) != date_ms)
        .and_then(|s| datalib_time::IsoOffsetTimestamp::from_unix_millis(s * 1000))
    {
        let stamp = edited.to_rfc3339_secs();
        let day = stamp.split('T').next().unwrap_or(&stamp);
        body.markup(format!("*Edited {day}*"));
    }
    let text = body.markdown();
    let display = display_for(None, body.opening_words(), "Facebook post on another page");
    let item_id = ids::post_text(&owner.source_id, row_id, date_ms);
    let mut item = chat_item(
        item_id,
        owner.name.clone(),
        date_ms,
        (!text.is_empty()).then_some(text),
        attachments,
    );
    item.problems = unread_keys(v, &["fbid", "label_values", "media", "timestamp"], "");
    item.problems.extend(unread_labels(v, OTHER_PAGE_LABELS));
    one_item_chat(row_id, inputs, display, None, item, owner)
}

/// `📍 Name — address`, once per place: a check-in carries the same place
/// twice, with and without its page URL, and the one with the URL wins.
fn place_line(place: &Value, places: &mut Vec<String>) {
    let Some(name) = str_field(place, "name") else {
        return;
    };
    let name = escape_md_inline(name);
    let mut line = match str_field(place, "url") {
        Some(url) => format!("📍 [{name}]({})", md_link_dest(url)),
        None => format!("📍 {name}"),
    };
    if let Some(addr) = str_field(place, "address") {
        line.push_str(&format!(" — {}", escape_md_inline(addr)));
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
/// the first line of what the person wrote, when the post opens with it,
/// is better.
fn display_for(title: Option<&str>, opening_words: Option<&str>, fallback: &str) -> String {
    let snippet = opening_words.map(first_line).unwrap_or_default();
    if !snippet.is_empty() {
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
        contacts: Vec::new(),
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
        upstream_account: None,
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
    use datalib_etl_chat_common::types::ItemKind;
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
        let chats = build_posts(&[("r1".to_string(), post)], &[], &[], &owner());
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

    /// What the person wrote and the place's name are text: escaped on
    /// the page, as written in the thread's title, which escapes itself.
    #[test]
    fn a_post_in_markup_renders_escaped() {
        let post = json!({
            "timestamp": 1,
            "attachments": [{"data": [{"place": {
                "name": "<script>x</script> & co",
                "url": "https://www.facebook.com/pages/x/1",
            }}]}],
            "data": [{"post": "<script>x</script> & co"}],
        });
        let chats = build_posts(&[("r1".to_string(), post)], &[], &[], &owner());
        assert_eq!(
            chats[0].buckets[0].items[0].text.as_deref(),
            Some(
                "&lt;script&gt;x&lt;/script&gt; &amp; co\n\n\
                 📍 [&lt;script&gt;x&lt;/script&gt; &amp; co](https://www.facebook.com/pages/x/1)"
            )
        );
        assert_eq!(chats[0].display, "<script>x</script> & co");
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
        let chats = build_posts(&[("r1".to_string(), post)], &[], &[], &owner());
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
        let chats = build_posts(&[("r1".to_string(), post)], &[], &[], &owner());
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
        let chats = build_posts(&[], &[("400000000000001".to_string(), post)], &[], &owner());
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
