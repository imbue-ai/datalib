//! What every Facebook feed's render shares: reading the export's record
//! shapes (`timestamp`, `data[]`, `attachments[].data[]`, `label_values`),
//! the mention markup, and one attachment per media file.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable as _;
use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::NormalizedAttachment;
use datalib_etl_facebook::ingest::schema_raw::MediaBlobRow;
use datalib_etl_render::inputs::Inputs;
use datalib_schema::providers::Provider;
use serde_json::Value;

/// Bump when the item shape or column mapping changes meaningfully.
/// v2: ids are minted through `datalib_id`, every row carries its
///     backpointer, and an item's id carries its stamp in its leading
///     bits (`datalib_id`'s v8 layout). Every uuid moved.
pub const RENDER_VERSION: u32 = 2;

pub const SOURCE_LABEL: &str = "Facebook";

/// `chat_entity_kind` is the `datalib_id` kind of the chat's own id — a
/// post's, an album's, a feed's.
pub fn profile(
    chat_kind: &str,
    message_kind: &str,
    chat_entity_kind: &'static str,
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

pub fn month_of(ms: Option<i64>) -> String {
    use chrono::TimeZone;
    ms.and_then(|ms| chrono::Utc.timestamp_millis_opt(ms).single())
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| "undated".to_string())
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
        assert_eq!(month_of(None), "undated");
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
