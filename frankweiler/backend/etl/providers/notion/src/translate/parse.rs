//! Read the event-store JSONL layout written by
//! [`crate::extract`] (port of `src/ingest/providers/notion_official/parse.py`).
//!
//! Layout:
//! ```text
//! <api_dir>/notion_official_page/{created,updated}/events.jsonl
//! <api_dir>/notion_official_block/{created,updated}/events.jsonl
//! <api_dir>/notion_official_comment/{created,updated}/events.jsonl
//! ```
//!
//! We also opportunistically read two unofficial-API tables when present
//! (these are written by the legacy unofficial downloader, not by the
//! Rust port — kept here so a backfilled backup directory still gives
//! display names + media URLs):
//! - `notion_user` — for comment-author display names.
//! - `notion_block` — for `prod-files-secure` media URLs and bookmark
//!   titles that the official API leaves blank.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_BLOCK: &str = "notion_official_block";
pub const ENTITY_COMMENT: &str = "notion_official_comment";

#[derive(Debug, Default, Clone)]
pub struct ParsedNotionOfficial {
    pub pages: Vec<Value>,
    pub blocks: Vec<Value>,
    pub comments: Vec<Value>,
    pub user_names: HashMap<String, String>,
    pub media_urls: HashMap<String, String>,
    pub bookmark_titles: HashMap<String, String>,
}

fn load_latest_raw(api_dir: &Path, entity: &str) -> Result<Vec<Value>> {
    let mut latest: HashMap<String, Value> = HashMap::new();
    for stream in ["created", "updated"] {
        let path = api_dir.join(entity).join(stream).join("events.jsonl");
        if !path.exists() {
            continue;
        }
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let rdr = BufReader::new(f);
        for (i, line) in rdr.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), i + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse {}:{}", path.display(), i + 1))?;
            let Some(id) = rec.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let raw = rec
                .get("raw")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            latest.insert(id.to_string(), raw);
        }
    }
    Ok(latest.into_values().collect())
}

/// Pull `value.value.<inner>` out of an unofficial-API record payload.
/// The unofficial wire format wraps the actual record under `value.value`
/// inside each `raw`.
fn unofficial_inner(raw: &Value) -> Option<&Value> {
    let val = raw.get("value")?;
    val.get("value").or(Some(val))
}

fn user_names_from_unofficial(api_dir: &Path) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::new();
    for stream in ["created", "updated"] {
        let path = api_dir
            .join("notion_user")
            .join(stream)
            .join("events.jsonl");
        if !path.exists() {
            continue;
        }
        let f = File::open(&path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let raw = rec
                .get("raw")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let val = unofficial_inner(&raw).cloned().unwrap_or(Value::Null);
            let uid = val
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| rec.get("id").and_then(|v| v.as_str()))
                .unwrap_or("");
            let name = val
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| val.get("given_name").and_then(|v| v.as_str()))
                .unwrap_or("");
            if !uid.is_empty() && !name.is_empty() {
                out.insert(uid.to_string(), name.to_string());
            }
        }
    }
    Ok(out)
}

fn first_string_prop(props: &Value, key: &str) -> String {
    // notion2: properties[key] is `[[text, annotations], ...]`.
    if let Some(arr) = props.get(key).and_then(|v| v.as_array()) {
        if let Some(first) = arr.first().and_then(|v| v.as_array()) {
            if let Some(s) = first.first().and_then(|v| v.as_str()) {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn block_lookups_from_unofficial(
    api_dir: &Path,
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let media_types: &[&str] = &["image", "video", "audio", "pdf", "file"];
    let mut media_urls: HashMap<String, String> = HashMap::new();
    let mut bookmark_titles: HashMap<String, String> = HashMap::new();

    for stream in ["created", "updated"] {
        let path = api_dir
            .join("notion_block")
            .join(stream)
            .join("events.jsonl");
        if !path.exists() {
            continue;
        }
        let f = File::open(&path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let raw = rec
                .get("raw")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let val = unofficial_inner(&raw).cloned().unwrap_or(Value::Null);
            let t = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let bid = val
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| rec.get("id").and_then(|v| v.as_str()))
                .unwrap_or("");
            if bid.is_empty() {
                continue;
            }
            let props = val
                .get("properties")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            if media_types.contains(&t) {
                let url = first_string_prop(&props, "source");
                if !url.is_empty() {
                    media_urls.insert(bid.to_string(), url);
                }
            } else if t == "bookmark" {
                let title = first_string_prop(&props, "title");
                if !title.is_empty() {
                    bookmark_titles.insert(bid.to_string(), title);
                }
            }
        }
    }
    Ok((media_urls, bookmark_titles))
}

pub fn parse_api_dir(api_dir: &Path) -> Result<ParsedNotionOfficial> {
    let (media_urls, bookmark_titles) = block_lookups_from_unofficial(api_dir)?;
    Ok(ParsedNotionOfficial {
        pages: load_latest_raw(api_dir, ENTITY_PAGE)?,
        blocks: load_latest_raw(api_dir, ENTITY_BLOCK)?,
        comments: load_latest_raw(api_dir, ENTITY_COMMENT)?,
        user_names: user_names_from_unofficial(api_dir)?,
        media_urls,
        bookmark_titles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn write_line(p: &Path, v: &Value) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(v).unwrap()).unwrap();
    }

    #[test]
    fn parse_picks_latest_per_id() {
        let d = tempdir().unwrap();
        let p = d.path();
        let created = p.join("notion_official_page/created/events.jsonl");
        let updated = p.join("notion_official_page/updated/events.jsonl");
        write_line(&created, &json!({"id": "abc", "raw": {"v": 1}}));
        write_line(&updated, &json!({"id": "abc", "raw": {"v": 1}}));
        write_line(&updated, &json!({"id": "abc", "raw": {"v": 2}}));
        write_line(&updated, &json!({"id": "xyz", "raw": {"v": 9}}));
        let out = parse_api_dir(p).unwrap();
        assert_eq!(out.pages.len(), 2);
        let by_v: std::collections::HashMap<_, _> = out
            .pages
            .iter()
            .map(|r| (r["v"].as_i64().unwrap(), 1))
            .collect();
        // abc → v=2 (updated stream wins), xyz → v=9
        assert!(by_v.contains_key(&2));
        assert!(by_v.contains_key(&9));
        assert!(!by_v.contains_key(&1));
    }
}
