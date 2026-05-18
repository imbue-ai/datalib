//! ChatGPT HTTP fixture synthesizer.
//!
//! Reads the snapshot layout the live downloader writes
//! (`<api_dir>/me.json`, `conversations.json`,
//! `conversations/<id>.json`) and emits playback fixtures matching
//! every request [`crate::extract::api::ChatGPTClient`] would issue:
//!
//! * `GET /backend-api/me`
//! * `GET /backend-api/conversations?offset=N&limit=100&order=updated`
//!   — one fixture per page slice plus a terminating empty page so the
//!   listing loop converges even when `total` is missing.
//! * `GET /backend-api/conversation/{id}` — one per id, with the two
//!   downloader-side synthetic keys (`_fetched_at`,
//!   `_listing_update_time`) stripped so the response looks like the
//!   live API rather than the on-disk cache.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

/// Matches `extract::PAGE_SIZE`. Hard-coded rather than imported so the
/// synthesizer doesn't drag in extract's tokio/tracing deps just for a
/// constant; the test below pins them together.
const PAGE_SIZE: usize = 100;

const BASE: &str = "https://chatgpt.com";

pub struct ChatgptSynth {
    pub api_dir: PathBuf,
}

impl ChatgptSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("chatgpt", url).header("Accept", "application/json")
}

fn strip_synthetic_keys(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("_fetched_at");
        obj.remove("_listing_update_time");
    }
    v
}

impl Synthesizer for ChatgptSynth {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let mut count = 0usize;

        let me_path = self.api_dir.join("me.json");
        if me_path.exists() {
            let me: Value = serde_json::from_slice(&fs::read(&me_path)?)
                .with_context(|| format!("parse {}", me_path.display()))?;
            let req = req_get(&format!("{BASE}/backend-api/me"));
            write_fixture(out_root, &req, &json_response(&me))?;
            count += 1;
        }

        let listing_path = self.api_dir.join("conversations.json");
        let listing: Vec<Value> = if listing_path.exists() {
            serde_json::from_slice(&fs::read(&listing_path)?)
                .with_context(|| format!("parse {}", listing_path.display()))?
        } else {
            Vec::new()
        };
        let total = listing.len() as u64;
        let mut offset = 0usize;
        while offset < listing.len() {
            let end = (offset + PAGE_SIZE).min(listing.len());
            let chunk: Vec<Value> = listing[offset..end].to_vec();
            let body = json!({"items": chunk, "total": total});
            let url = format!(
                "{BASE}/backend-api/conversations?offset={offset}&limit={PAGE_SIZE}&order=updated"
            );
            write_fixture(out_root, &req_get(&url), &json_response(&body))?;
            count += 1;
            offset = end;
        }
        // Terminating empty page; harmless if total honestly bounds the
        // loop, but required when total is missing from the snapshot.
        let url = format!(
            "{BASE}/backend-api/conversations?offset={offset}&limit={PAGE_SIZE}&order=updated"
        );
        let body = json!({"items": [], "total": total});
        write_fixture(out_root, &req_get(&url), &json_response(&body))?;
        count += 1;

        let convs_dir = self.api_dir.join("conversations");
        if convs_dir.is_dir() {
            let mut entries: Vec<PathBuf> = fs::read_dir(&convs_dir)
                .with_context(|| format!("read {}", convs_dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            entries.sort();
            for path in entries {
                let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let v: Value = serde_json::from_slice(&fs::read(&path)?)
                    .with_context(|| format!("parse {}", path.display()))?;
                let stripped = strip_synthetic_keys(v);
                let url = format!("{BASE}/backend-api/conversation/{id}");
                write_fixture(out_root, &req_get(&url), &json_response(&stripped))?;
                count += 1;
            }
        }

        Ok(SynthesizeReport {
            fixtures_written: count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankweiler_etl::http::{fixture_key, HttpResponse};
    use serde_json::Map;
    use tempfile::tempdir;

    #[test]
    fn page_size_matches_extract() {
        assert_eq!(PAGE_SIZE, crate::extract::PAGE_SIZE);
    }

    fn write(path: &Path, v: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(v).unwrap()).unwrap();
    }

    #[test]
    fn emits_me_listing_pages_and_per_conv_fixtures() {
        let d = tempdir().unwrap();
        let api = d.path().join("chatgpt_api");
        write(&api.join("me.json"), &json!({"id": "u1", "email": "x@y"}));
        // 101 items -> two pages (100 + 1) + one terminating empty.
        let items: Vec<Value> = (0..101)
            .map(|i| json!({"id": format!("c{i}"), "update_time": i}))
            .collect();
        write(&api.join("conversations.json"), &Value::Array(items));
        write(
            &api.join("conversations/c0.json"),
            &json!({
                "id": "c0",
                "mapping": {},
                "_fetched_at": "2025-01-01T00:00:00Z",
                "_listing_update_time": 0
            }),
        );

        let out = d.path().join("playback");
        let synth = ChatgptSynth::new(&api);
        let report = synth.synthesize(&out).unwrap();
        // me + 2 listing pages + 1 terminator + 1 conv = 5
        assert_eq!(report.fixtures_written, 5);

        let me_req = req_get(&format!("{BASE}/backend-api/me"));
        let me_path = out.join("chatgpt").join(fixture_key(&me_req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(me_path).unwrap()).unwrap();
        assert_eq!(resp.status, 200);

        // Per-conv synthetic keys must be stripped from the served body.
        let conv_req = req_get(&format!("{BASE}/backend-api/conversation/c0"));
        let conv_path = out.join("chatgpt").join(fixture_key(&conv_req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(conv_path).unwrap()).unwrap();
        let body: Map<String, Value> = serde_json::from_slice(&resp.body).unwrap();
        assert!(!body.contains_key("_fetched_at"));
        assert!(!body.contains_key("_listing_update_time"));
        assert_eq!(body.get("id").unwrap(), "c0");
    }
}
