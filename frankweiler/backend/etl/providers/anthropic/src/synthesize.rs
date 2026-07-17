//! Anthropic (claude.ai) HTTP fixture synthesizer.
//!
//! Reads the snapshot layout the live downloader writes — `<api_dir>/
//! conversations.json` (post-normalize array of full conversations) and
//! `users.json` — and emits playback fixtures for every request
//! [`crate::download::api::ClaudeClient`] would issue:
//!
//! * `GET /organizations` — reconstructed from the `account.uuid` /
//!   `org_uuid` fields embedded in the stored conversations.
//! * `GET /organizations/{org}/chat_conversations` — listing per org,
//!   stripped down to `{uuid, name, summary, updated_at}`-ish shape.
//! * `GET /organizations/{org}/chat_conversations/{conv}?tree=True&...`
//!   — per-conversation detail. We serve the normalized form back; the
//!   downstream `normalize_to_export_shape` pass is idempotent on
//!   already-normalized input (text/account fields are added only when
//!   absent), so playback re-runs converge.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

const BASE: &str = "https://claude.ai/api";
const DETAIL_QUERY: &str =
    "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong";

pub struct AnthropicSynth {
    pub api_dir: PathBuf,
}

impl AnthropicSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("anthropic", url).header("Accept", "application/json")
}

fn org_uuid_of(conv: &Value) -> Option<String> {
    let direct = conv
        .get("organization_uuid")
        .or_else(|| conv.get("organization").and_then(|o| o.get("uuid")))
        .and_then(|v| v.as_str());
    if let Some(s) = direct {
        return Some(s.to_string());
    }
    conv.get("_source")
        .and_then(|s| s.get("org_uuid"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn listing_item(conv: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for key in [
        "uuid",
        "name",
        "summary",
        "created_at",
        "updated_at",
        "model",
    ] {
        if let Some(v) = conv.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Value::Object(obj)
}

impl Synthesizer for AnthropicSynth {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let convs_path = self.api_dir.join("conversations.json");
        let convs: Vec<Value> = if convs_path.exists() {
            let raw: Value = serde_json::from_slice(&fs::read(&convs_path)?)
                .with_context(|| format!("parse {}", convs_path.display()))?;
            raw.as_array().cloned().unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut by_org: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for c in &convs {
            if let Some(org) = org_uuid_of(c) {
                by_org.entry(org).or_default().push(c);
            }
        }

        let mut count = 0usize;

        // /organizations
        let orgs: Vec<Value> = by_org
            .keys()
            .map(|uuid| json!({"uuid": uuid, "name": uuid}))
            .collect();
        let req = req_get(&format!("{BASE}/organizations"));
        write_fixture(out_root, &req, &json_response(&Value::Array(orgs)))?;
        count += 1;

        for (org, items) in &by_org {
            // Listing.
            let listing: Vec<Value> = items.iter().map(|c| listing_item(c)).collect();
            let req = req_get(&format!("{BASE}/organizations/{org}/chat_conversations"));
            write_fixture(out_root, &req, &json_response(&Value::Array(listing)))?;
            count += 1;

            // Detail per conversation.
            for c in items {
                let Some(uuid) = c.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                let url =
                    format!("{BASE}/organizations/{org}/chat_conversations/{uuid}?{DETAIL_QUERY}");
                write_fixture(out_root, &req_get(&url), &json_response(c))?;
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
    use tempfile::tempdir;

    #[test]
    fn emits_orgs_listings_and_details() {
        let d = tempdir().unwrap();
        let api = d.path().join("anthropic_api");
        fs::create_dir_all(&api).unwrap();
        let convs = json!([
            {
                "uuid": "c1", "name": "First", "updated_at": "2025-01-01T00:00:00Z",
                "organization_uuid": "org-a", "chat_messages": []
            },
            {
                "uuid": "c2", "name": "Second", "updated_at": "2025-01-02T00:00:00Z",
                "organization": {"uuid": "org-a"}, "chat_messages": []
            },
            {
                "uuid": "c3", "name": "Third", "updated_at": "2025-01-03T00:00:00Z",
                "organization_uuid": "org-b", "chat_messages": []
            }
        ]);
        fs::write(
            api.join("conversations.json"),
            serde_json::to_vec(&convs).unwrap(),
        )
        .unwrap();

        let out = d.path().join("playback");
        let report = AnthropicSynth::new(&api).synthesize(&out).unwrap();
        // /organizations + 2 listings + 3 detail = 6
        assert_eq!(report.fixtures_written, 6);

        let orgs_req = req_get(&format!("{BASE}/organizations"));
        let p = out.join("anthropic").join(fixture_key(&orgs_req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        let names: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.get("uuid").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["org-a", "org-b"]);

        let detail_req = req_get(&format!(
            "{BASE}/organizations/org-a/chat_conversations/c1?{DETAIL_QUERY}"
        ));
        let p = out.join("anthropic").join(fixture_key(&detail_req));
        assert!(p.exists(), "missing detail fixture at {}", p.display());
    }
}
