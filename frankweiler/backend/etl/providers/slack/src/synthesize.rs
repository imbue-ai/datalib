//! Slack HTTP fixture synthesizer.
//!
//! Reads the raw-API JSONL the live downloader writes under
//! `<api_dir>/raw_api/<method>/*.jsonl` — each line is a recorded
//! `{method, params, response, ...}` envelope — and emits one playback
//! fixture per recorded call. Because every paginated request was
//! captured individually with its own `cursor`/`oldest`/`latest` params,
//! the cursor chain is preserved for free: replaying just hits the same
//! URLs in the same order and the live extract walks them.
//!
//! Methods covered (matches [`crate::extract::shapes`]):
//! `auth.test`, `users.list`, `conversations.list`,
//! `conversations.history`, `conversations.replies`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::Value;

use crate::extract::api::build_url;

pub struct SlackSynth {
    pub api_dir: PathBuf,
}

impl SlackSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_for(method: &str, params: &BTreeMap<String, String>) -> HttpRequest {
    HttpRequest::get("slack", build_url(method, params))
}

fn parse_params(rec: &Value) -> BTreeMap<String, String> {
    rec.get("params")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

impl Synthesizer for SlackSynth {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let raw_root = self.api_dir.join("raw_api");
        if !raw_root.is_dir() {
            return Ok(SynthesizeReport::default());
        }
        let mut count = 0usize;
        let mut method_dirs: Vec<PathBuf> = fs::read_dir(&raw_root)
            .with_context(|| format!("read {}", raw_root.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        method_dirs.sort();

        for method_dir in method_dirs {
            let method = method_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if method.is_empty() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(&method_dir)
                .with_context(|| format!("read {}", method_dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            files.sort();
            for path in files {
                let f =
                    fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
                for (i, line) in BufReader::new(f).lines().enumerate() {
                    let line =
                        line.with_context(|| format!("read {}:{}", path.display(), i + 1))?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let rec: Value = serde_json::from_str(&line)
                        .with_context(|| format!("parse {}:{}", path.display(), i + 1))?;
                    let params = parse_params(&rec);
                    let response = rec.get("response").cloned().unwrap_or(Value::Null);
                    let req = req_for(&method, &params);
                    write_fixture(out_root, &req, &json_response(&response))?;
                    count += 1;
                }
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
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn emits_one_fixture_per_recorded_call() {
        let d = tempdir().unwrap();
        let api = d.path().join("slack_api");
        let auth_dir = api.join("raw_api/auth.test");
        fs::create_dir_all(&auth_dir).unwrap();
        fs::write(
            auth_dir.join("run-1.jsonl"),
            serde_json::to_string(&json!({
                "method": "auth.test",
                "params": {},
                "response": {"ok": true, "user_id": "U1"}
            }))
            .unwrap(),
        )
        .unwrap();

        let hist_dir = api.join("raw_api/conversations.history");
        fs::create_dir_all(&hist_dir).unwrap();
        let mut hist = String::new();
        hist.push_str(&serde_json::to_string(&json!({
            "method": "conversations.history",
            "params": {"channel": "C1", "limit": "100"},
            "response": {"ok": true, "messages": [{"ts": "1.0"}], "response_metadata": {"next_cursor": "c2"}}
        })).unwrap());
        hist.push('\n');
        hist.push_str(
            &serde_json::to_string(&json!({
                "method": "conversations.history",
                "params": {"channel": "C1", "limit": "100", "cursor": "c2"},
                "response": {"ok": true, "messages": [{"ts": "2.0"}]}
            }))
            .unwrap(),
        );
        fs::write(hist_dir.join("run-1.jsonl"), hist).unwrap();

        let out = d.path().join("playback");
        let report = SlackSynth::new(&api).synthesize(&out).unwrap();
        assert_eq!(report.fixtures_written, 3);

        // Spot-check that the second history page fixture is keyed by
        // the URL including the cursor param.
        let mut params = BTreeMap::new();
        params.insert("channel".to_string(), "C1".to_string());
        params.insert("limit".to_string(), "100".to_string());
        params.insert("cursor".to_string(), "c2".to_string());
        let req = req_for("conversations.history", &params);
        let p = out.join("slack").join(fixture_key(&req));
        assert!(p.exists());
        let resp: HttpResponse = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["messages"][0]["ts"], "2.0");
    }
}
