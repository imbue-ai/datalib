//! What every Facebook feed's render shares: reading the export's record
//! shapes (`timestamp`, `data[]`, `attachments[].data[]`, `label_values`),
//! the mention markup, and one attachment per media file.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable as _;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChatItem, UpstreamRef,
};
use datalib_etl_facebook::ingest::schema_raw::MediaBlobRow;
use datalib_etl_render::inputs::Inputs;
use datalib_id::Identity;
use datalib_schema::problems::{Problem, Reason, Severity};
use datalib_schema::providers::Provider;
use serde_json::Value;

/// Bump when the item shape or column mapping changes meaningfully.
/// v2: ids are minted through `datalib_id`, every row carries its
///     backpointer, and an item's id carries its stamp in its leading
///     bits (`datalib_id`'s v8 layout). Every uuid moved.
/// v4: the comments and reactions feeds are one document per year.
/// v5: a friend is their own conversation, and the friends list is the
///     channel alone.
/// v6: Messenger conversations, one document per year; a comment's link;
///     a problem row for every field render leaves unread.
pub const RENDER_VERSION: u32 = 6;

pub const SOURCE_LABEL: &str = "Facebook";

/// `chat_entity_kind` is the `datalib_id` kind of the chat's own id — a
/// post's, an album's, a feed's.
pub fn profile(
    chat_kind: &str,
    message_kind: &str,
    chat_entity_kind: &'static str,
    text_format: TextFormat,
) -> RenderProfile {
    RenderProfile {
        stamp_precision: crate::ids::STAMP_PRECISION,
        provider: Provider::Facebook,
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: chat_kind.to_string(),
        message_kind: message_kind.to_string(),
        reaction_kind: "Facebook Reaction".to_string(),
        chat_entity_kind,
        render_version: RENDER_VERSION,
        text_format,
    }
}

pub fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A record's `timestamp` — seconds since the epoch in every export
/// file — as unix milliseconds. `None` for a missing or zero stamp,
/// which is how the export spells "unknown" (`"timestamp": 0`).
pub fn ts_ms(v: &Value, key: &str) -> Option<i64> {
    v.get(key)
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .map(|s| s * 1000)
}

/// `data[]` entries are one-key objects; this is every entry's `key`.
pub fn data_values<'a>(record: &'a Value, key: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    record
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(move |d| d.get(key))
}

/// Every `attachments[].data[]` entry, flattened.
pub fn attachment_entries(record: &Value) -> impl Iterator<Item = &Value> {
    record
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("data").and_then(Value::as_array))
        .flatten()
}

/// The `label_values` shape: `value` of the entry whose `label` is `label`.
pub fn label_value<'a>(record: &'a Value, label: &str) -> Option<&'a Value> {
    record
        .get("label_values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|lv| lv.get("label").and_then(Value::as_str) == Some(label))
}

/// Facebook writes a tagged person into text as `@[<id>:2048:<Name>]` —
/// or, in a life event's description, with the `@[<id>:` already gone
/// and a bare `2048:<Name>` left at the start of a line. A reader wants
/// the name either way.
pub fn strip_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("@[") {
        let Some(len) = rest[start..].find(']') else {
            break;
        };
        let inner = &rest[start + 2..start + len];
        out.push_str(&rest[..start]);
        out.push_str(inner.rsplit(':').next().unwrap_or(inner));
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.lines()
        .map(|l| l.strip_prefix("2048:").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One attachment for a `media` object: the export-relative `uri` is the
/// CAS ref, and the bytes are resolved through the chat's bundle. The
/// `media_blobs` edge from `owner_id` (the record's row) to the uri is
/// declared read, so the bytes arriving, changing or going re-renders
/// the document that shows them.
pub fn media_attachment(
    media: &Value,
    owner_id: &str,
    inputs: &Inputs,
) -> Option<NormalizedAttachment> {
    let uri = str_field(media, "uri")?;
    inputs.read(MediaBlobRow::TABLE, &MediaBlobRow::pk_recipe(owner_id, uri));
    Some(NormalizedAttachment {
        rel_path: None,
        file_name: uri.rsplit('/').next().map(str::to_string),
        mime_type: mime_for(uri),
        byte_len: None,
        source_url: None,
        ref_id: Some(uri.to_string()),
    })
}

/// A media object's own caption: its `description`, else its `title` —
/// unless that is just the album's name repeated, which every album
/// photo carries.
pub fn media_caption(media: &Value, album_name: Option<&str>) -> Option<String> {
    str_field(media, "description")
        .or_else(|| str_field(media, "title").filter(|t| Some(*t) != album_name))
        .map(strip_mentions)
}

/// One item of a feed: an attachment item when it carries any, else a
/// text one.
pub fn chat_item(
    item_id: Identity,
    author_display: String,
    date_ms: Option<i64>,
    text: Option<String>,
    attachments: Vec<NormalizedAttachment>,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: item_id.uuid,
        author_handle: None,
        author_display,
        date_ms,
        text,
        kind: if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        },
        attachments,
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: Some(UpstreamRef::new(item_id.entity_kind, item_id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems: Vec::new(),
    }
}

fn mime_for(uri: &str) -> Option<String> {
    let ext = uri.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        _ => return None,
    };
    Some(mime.to_string())
}

/// A warning for each key of `v` this render does not read, so a run over
/// a real export says what it left out. The sample is the value's JSON
/// shape, never its content: these rows are read to learn the export's
/// shapes, and the export is private.
pub fn unread_keys(v: &Value, read: &[&str], path: &str) -> Vec<Problem> {
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(k, _)| !read.contains(&k.as_str()))
        .map(|(k, v)| {
            Problem::field(k.clone(), Reason::UncoveredType, &shape_of(v))
                .at(format!("{path}/{k}"))
                .severity(Severity::Warning)
        })
        .collect()
}

/// The keys of a `media` object this render reads or leaves out on
/// purpose (`media_metadata` is the camera's EXIF and the upload IP).
pub const MEDIA_KEYS: &[&str] = &[
    "uri",
    "creation_timestamp",
    "title",
    "description",
    "media_metadata",
    "dubbing_info",
    "media_variants",
    "ai_stickers",
    "backup_uri",
];

/// The `attachments[].data[]` entries of a record, each checked against
/// what render reads of its one key: `media`, `place`, `life_event`,
/// `external_context` and `text`, and nothing else.
pub fn unread_attachment_keys(record: &Value) -> Vec<Problem> {
    let mut out = Vec::new();
    let attachments = record.get("attachments").and_then(Value::as_array);
    for (a, attachment) in attachments.into_iter().flatten().enumerate() {
        let path = format!("/attachments/{a}");
        out.extend(unread_keys(attachment, &["data"], &path));
        let entries = attachment.get("data").and_then(Value::as_array);
        for (e, entry) in entries.into_iter().flatten().enumerate() {
            let path = format!("{path}/data/{e}");
            out.extend(unread_keys(
                entry,
                &["media", "place", "life_event", "external_context", "text"],
                &path,
            ));
            let nested: [(&str, &[&str]); 4] = [
                ("media", MEDIA_KEYS),
                ("place", &["name", "url", "address", "coordinate"]),
                (
                    "life_event",
                    &["title", "description", "place", "start_date"],
                ),
                ("external_context", &["name", "url", "source"]),
            ];
            for (key, read) in nested {
                if let Some(v) = entry.get(key) {
                    out.extend(unread_keys(v, read, &format!("{path}/{key}")));
                }
            }
        }
    }
    out
}

/// The `label_values` entries whose `label` (or a section's `title`)
/// render does not read, each reported under `label_values:<label>` by
/// the shape of its entry. An entry with nothing in it — no value, an
/// empty list — loses nothing, and is not one; a real export is full of
/// them.
pub fn unread_labels(record: &Value, read: &[&str]) -> Vec<Problem> {
    let entries = record.get("label_values").and_then(Value::as_array);
    entries
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, lv)| {
            // A section of nested entries has a `title` where an entry
            // has its `label`.
            let label = lv
                .get("label")
                .or_else(|| lv.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (!read.contains(&label) && !is_empty_entry(lv)).then(|| {
                Problem::field(
                    format!("label_values:{label}"),
                    Reason::UncoveredType,
                    &shape_of(lv),
                )
                .at(format!("/label_values/{i}"))
                .severity(Severity::Warning)
            })
        })
        .collect()
}

/// A `label_values` entry with no value: every key but its `label` or
/// `title` empty, or absent.
fn is_empty_entry(lv: &Value) -> bool {
    lv.as_object().is_some_and(|m| {
        m.iter()
            .filter(|(k, _)| *k != "label" && *k != "title")
            .all(|(_, v)| match v {
                Value::Null => true,
                Value::String(s) => s.trim().is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(o) => o.is_empty(),
                _ => false,
            })
    })
}

/// What a value is, without what it says: `string(12 chars)`,
/// `array(3)`, `object{a,b}`, `int`, `bool:true`.
pub fn shape_of(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) if n.is_i64() || n.is_u64() => "int".to_string(),
        Value::Number(_) => "float".to_string(),
        Value::String(s) => format!("string({} chars)", s.chars().count()),
        Value::Array(a) => format!("array({})", a.len()),
        Value::Object(m) => {
            let keys: Vec<&str> = m.keys().map(String::as_str).collect();
            format!("object{{{}}}", keys.join(","))
        }
    }
}

/// A finding about a record that cost it nothing, worth counting on a
/// real export.
pub fn noted(field: &str, explanation: &str) -> Problem {
    Problem::explained(Reason::Noted, Some(field.to_string()), explanation).severity(Severity::Info)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mentions_become_names() {
        assert_eq!(
            strip_mentions(
                "Farpoint, from orbit. @[170100000000002:2048:Data] and @[1:2048:Will Riker]!"
            ),
            "Farpoint, from orbit. Data and Will Riker!"
        );
        assert_eq!(strip_mentions("no mentions"), "no mentions");
        // An unterminated one is left as written.
        assert_eq!(
            strip_mentions("broken @[1:2048:Data"),
            "broken @[1:2048:Data"
        );
        // The life-event form, seen in a real export.
        assert_eq!(
            strip_mentions("Project Data Liberation is on!\n\n2048:Thad Hughes"),
            "Project Data Liberation is on!\n\nThad Hughes"
        );
    }

    #[test]
    fn zero_timestamp_is_unknown() {
        assert_eq!(ts_ms(&json!({"timestamp": 0}), "timestamp"), None);
        assert_eq!(ts_ms(&json!({"timestamp": 12}), "timestamp"), Some(12_000));
        assert_eq!(ts_ms(&json!({}), "timestamp"), None);
    }

    #[test]
    fn album_name_is_not_a_caption() {
        let m = json!({"uri": "a/b.jpg", "title": "Ten Forward Nights"});
        assert_eq!(media_caption(&m, Some("Ten Forward Nights")), None);
        assert_eq!(
            media_caption(&m, None).as_deref(),
            Some("Ten Forward Nights")
        );
        let d = json!({"uri": "a/b.jpg", "title": "T", "description": "Guinan @[1:2048:Guinan]"});
        assert_eq!(media_caption(&d, None).as_deref(), Some("Guinan Guinan"));
    }

    #[test]
    fn an_unread_key_is_reported_by_its_shape_alone() {
        let v = json!({"content": "private words", "call_duration": 42, "x": {"a": 1}});
        let problems = unread_keys(&v, &["content"], "/message");
        let mut got: Vec<(String, String, String)> = problems
            .iter()
            .map(|p| {
                (
                    p.field.clone().unwrap(),
                    p.sample.clone(),
                    p.path.clone().unwrap(),
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (
                    "call_duration".into(),
                    "int".into(),
                    "/message/call_duration".into()
                ),
                ("x".into(), "object{a}".into(), "/message/x".into()),
            ]
        );
        assert_eq!(shape_of(&json!("private words")), "string(13 chars)");
    }

    #[test]
    fn an_empty_label_values_entry_is_not_a_problem() {
        let r = json!({"label_values": [
            {"label": "Target"},
            {"label": "Files", "media": []},
            {"title": "Shares", "dict": []},
            {"label": "Feeling", "value": " "},
            {"label": "Mood", "value": "curious"},
        ]});
        let fields: Vec<String> = unread_labels(&r, &[])
            .into_iter()
            .filter_map(|p| p.field)
            .collect();
        assert_eq!(fields, ["label_values:Mood"]);
    }

    #[test]
    fn label_values_lookup() {
        let r = json!({"label_values": [
            {"label": "Reaction", "value": "Like"},
            {"label": "URL", "value": "https://x", "href": "https://x"},
        ]});
        assert_eq!(
            label_value(&r, "URL")
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str),
            Some("https://x")
        );
        assert!(label_value(&r, "Nope").is_none());
    }
}
