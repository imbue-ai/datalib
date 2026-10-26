//! What the download makes of Zoom's JSON, as pure functions: which
//! recording files a meeting owes and under what key, and the canonical
//! order a listing entry is stored in.

use serde_json::Value;

/// One recording file a meeting's listing entry names, that the config
/// wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedFile {
    pub meeting_uuid: String,
    pub file_key: String,
    pub file_type: String,
    /// Zoom's `file_size`, the version the file is held at: a transcript
    /// someone edited in Zoom's web portal comes back another size.
    pub size: Option<u64>,
    pub media: bool,
}

impl WantedFile {
    /// The `recording_files` key.
    pub fn key(&self) -> String {
        format!("{}#{}", self.meeting_uuid, self.file_key)
    }
}

/// Video and audio: a recording's big files, fetched only on request.
pub fn is_media(file_type: &str) -> bool {
    matches!(file_type, "MP4" | "M4A")
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The key a file is stored under within its meeting: Zoom's file id,
/// or for a closed-caption or timeline file, which Zoom gives no id, its
/// type in lower case.
pub fn file_key(file: &Value) -> Option<String> {
    match str_of(file, "id") {
        Some(id) => Some(id.to_string()),
        None => str_of(file, "file_type").map(str::to_ascii_lowercase),
    }
}

/// The files of a listed meeting that are owed bytes: every one with a
/// download link, the video and audio ones only when `media` is on. A
/// file Zoom is still processing has no link yet, and is owed once a
/// later listing gives it one.
pub fn wanted_files(meeting: &Value, media: bool) -> Vec<WantedFile> {
    let Some(uuid) = str_of(meeting, "uuid") else {
        return Vec::new();
    };
    meeting
        .get("recording_files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|f| str_of(f, "download_url").is_some())
        .filter(|f| str_of(f, "status").is_none_or(|s| s == "completed"))
        .filter_map(|f| {
            let file_type = str_of(f, "file_type").unwrap_or("").to_string();
            let is_media = is_media(&file_type);
            if is_media && !media {
                return None;
            }
            Some(WantedFile {
                meeting_uuid: uuid.to_string(),
                file_key: file_key(f)?,
                size: f.get("file_size").and_then(Value::as_u64),
                media: is_media,
                file_type,
            })
        })
        .collect()
}

/// The file of a meeting's fresh recordings answer stored under
/// `file_key`.
pub fn file_named<'a>(files: &'a [Value], key: &str) -> Option<&'a Value> {
    files.iter().find(|f| file_key(f).as_deref() == Some(key))
}

/// The MIME type a recording file is stored under, from its extension.
pub fn content_type(file: &Value) -> Option<&'static str> {
    Some(
        match str_of(file, "file_extension")?
            .to_ascii_uppercase()
            .as_str()
        {
            "VTT" => "text/vtt",
            "TXT" => "text/plain",
            "JSON" => "application/json",
            "CSV" => "text/csv",
            "MP4" => "video/mp4",
            "M4A" => "audio/mp4",
            "JPG" => "image/jpeg",
            _ => return None,
        },
    )
}

/// A listing entry with its recording files in a fixed order, so an
/// unchanged meeting listed again stores the same bytes.
pub fn canonical_recording(mut meeting: Value) -> Value {
    for key in ["recording_files", "participant_audio_files"] {
        if let Some(files) = meeting.get_mut(key).and_then(Value::as_array_mut) {
            files.sort_by_key(|f| {
                (
                    file_key(f),
                    str_of(f, "recording_start").map(str::to_string),
                )
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

    fn meeting() -> Value {
        json!({"uuid": "Ab1/cd==", "recording_files": [
            {"id": "f-mp4", "file_type": "MP4", "file_size": 900, "status": "completed",
             "download_url": "https://zoom.us/rec/download/a"},
            {"id": "f-vtt", "file_type": "TRANSCRIPT", "file_size": 40, "status": "completed",
             "download_url": "https://zoom.us/rec/download/b"},
            {"file_type": "TIMELINE", "download_url": "https://zoom.us/rec/download/c"},
            {"id": "f-busy", "file_type": "CHAT", "status": "processing"},
        ]})
    }

    #[test]
    fn media_is_left_out_unless_asked_for() {
        let keys = |media| -> Vec<String> {
            wanted_files(&meeting(), media)
                .iter()
                .map(WantedFile::key)
                .collect()
        };
        assert_eq!(keys(false), ["Ab1/cd==#f-vtt", "Ab1/cd==#timeline"]);
        assert_eq!(
            keys(true),
            ["Ab1/cd==#f-mp4", "Ab1/cd==#f-vtt", "Ab1/cd==#timeline"]
        );
    }

    #[test]
    fn a_file_is_held_at_its_size() {
        let files = wanted_files(&meeting(), false);
        assert_eq!(files[0].size, Some(40));
        assert_eq!(files[1].size, None);
    }

    #[test]
    fn listing_order_does_not_change_what_is_stored() {
        let mut shuffled = meeting();
        shuffled["recording_files"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(
            canonical_recording(shuffled),
            canonical_recording(meeting())
        );
    }

    #[test]
    fn a_uuid_is_encoded_twice_only_where_zoom_asks() {
        assert_eq!(uuid_segment("Ab1+cd=="), "Ab1%2Bcd%3D%3D");
        assert_eq!(uuid_segment("/Ab1cd=="), "%252FAb1cd%253D%253D");
        assert_eq!(uuid_segment("Ab//cd=="), "Ab%252F%252Fcd%253D%253D");
    }
}
