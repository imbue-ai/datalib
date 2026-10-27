//! What the download makes of Zoom's JSON, as pure functions: the
//! canonical order a listing entry is stored in, and a meeting UUID as
//! a path segment.

use serde_json::Value;

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A listing entry with its recording files in a fixed order, so an
/// unchanged meeting listed again stores the same bytes. A file is
/// ordered by its id, or by its type and start where Zoom gives it no
/// id (a closed-caption or timeline file).
pub fn canonical_recording(mut meeting: Value) -> Value {
    for key in ["recording_files", "participant_audio_files"] {
        if let Some(files) = meeting.get_mut(key).and_then(Value::as_array_mut) {
            files.sort_by_key(|f| {
                let s = |k: &str| str_of(f, k).map(str::to_string);
                (s("id"), s("file_type"), s("recording_start"))
            });
        }
    }
    meeting
}

/// A meeting UUID as a path segment. Zoom's UUIDs are base64, so `/`,
/// `+` and `=` are percent-encoded, and one that starts with `/` or holds
/// `//` is encoded twice, as Zoom asks.
pub fn uuid_segment(uuid: &str) -> String {
    let once = datalib_etl_web::http::percent_encode(uuid);
    if uuid.starts_with('/') || uuid.contains("//") {
        datalib_etl_web::http::percent_encode(&once)
    } else {
        once
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn listing_order_does_not_change_what_is_stored() {
        let meeting = json!({"uuid": "Ab1/cd==", "recording_files": [
            {"id": "f-mp4", "file_type": "MP4"},
            {"id": "f-vtt", "file_type": "TRANSCRIPT"},
            {"file_type": "TIMELINE", "recording_start": "2026-10-05T09:01:00Z"},
            {"file_type": "TIMELINE", "recording_start": "2026-10-05T10:15:00Z"},
        ]});
        let mut shuffled = meeting.clone();
        shuffled["recording_files"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(canonical_recording(shuffled), canonical_recording(meeting));
    }

    #[test]
    fn a_uuid_is_encoded_twice_only_where_zoom_asks() {
        assert_eq!(uuid_segment("Ab1+cd=="), "Ab1%2Bcd%3D%3D");
        assert_eq!(uuid_segment("/Ab1cd=="), "%252FAb1cd%253D%253D");
        assert_eq!(uuid_segment("Ab//cd=="), "Ab%252F%252Fcd%253D%253D");
    }
}
