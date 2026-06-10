//! GitLab HTTP fixture synthesizer.
//!
//! Walks the event-store layout the live downloader writes under
//! `<api_dir>/<entity>/{created,updated}/events.jsonl` and emits playback
//! fixtures for every request [`crate::extract`] would issue:
//!
//! * `GET /api/v4/user` — viewer identity, from latest `self_identity`.
//! * `GET /api/v4/merge_requests?...` — one fixture per
//!   [`crate::extract::DEFAULT_SCOPES`] scope. First-run / full-sync
//!   assumption: no `updated_after` clause is appended. The body is a
//!   bare array of minimal items (`web_url` + `iid` are the only fields
//!   extract reads). No `Link: rel="next"` header → paginate stops.
//! * `GET /api/v4/projects/{url-encoded-path}/merge_requests/{iid}` — MR
//!   detail from latest `merge_request.raw`.
//! * `GET /api/v4/projects/{path}/merge_requests/{iid}/discussions?per_page=100`
//!   — array of `discussion.raw` per MR.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::event_store::load_latest_by_key;
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::Value;

use crate::extract::schema_raw::{discussion_pk_recipe, mr_pk_recipe};
use crate::extract::{BASE, DEFAULT_SCOPES, ENTITY_DISCUSSION, ENTITY_MR, ENTITY_SELF, PER_PAGE};

pub struct GitlabSynth {
    pub api_dir: PathBuf,
}

impl GitlabSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("gitlab", url)
}

fn mr_proj_iid(rec: &Value) -> Option<(String, u64)> {
    let p = rec.get("project_full_path")?.as_str()?.to_string();
    let n = rec.get("mr_iid")?.as_u64()?;
    Some((p, n))
}

impl Synthesizer for GitlabSynth {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        if !self.api_dir.is_dir() {
            return Ok(SynthesizeReport::default());
        }
        let mut count = 0usize;
        let mut user_id: i64 = 0;

        // /user from latest self_identity.
        let selves = load_latest_by_key(&self.api_dir, ENTITY_SELF, |r| {
            r.get("user_id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
                .unwrap_or_default()
        })?;
        if let Some(latest) = selves.values().next() {
            user_id = latest
                .get("raw")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_i64())
                .or_else(|| latest.get("user_id").and_then(|v| v.as_i64()))
                .unwrap_or(0);
            let raw = latest.get("raw").cloned().unwrap_or(Value::Null);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/user")),
                &json_response(&raw),
            )?;
            count += 1;
        }

        // MRs, grouped by (project_full_path, iid).
        let mrs = load_latest_by_key(&self.api_dir, ENTITY_MR, |r| {
            mr_proj_iid(r)
                .map(|(p, n)| mr_pk_recipe(&p, n as u32))
                .unwrap_or_default()
        })?;
        let mut mr_by_key: BTreeMap<(String, u64), Value> = BTreeMap::new();
        for rec in mrs.into_values() {
            if let Some(k) = mr_proj_iid(&rec) {
                if let Some(raw) = rec.get("raw").cloned() {
                    mr_by_key.insert(k, raw);
                }
            }
        }

        // Discovery search fixtures per default scope. Minimal item shape:
        // `web_url` (extract derives proj from it) + `iid`.
        let items: Vec<Value> = mr_by_key
            .iter()
            .map(|((proj, iid), raw)| {
                let web_url = raw.get("web_url").cloned().unwrap_or_else(|| {
                    Value::String(format!("https://gitlab.com/{proj}/-/merge_requests/{iid}"))
                });
                let mut obj = serde_json::Map::new();
                obj.insert("web_url".into(), web_url);
                obj.insert("iid".into(), Value::from(*iid));
                Value::Object(obj)
            })
            .collect();
        for scope in DEFAULT_SCOPES {
            let scope_param = if *scope == "reviewer" {
                format!("reviewer_id={user_id}")
            } else {
                format!("scope={scope}")
            };
            let url = format!(
                "{BASE}/merge_requests?{scope_param}&state=all&per_page={PER_PAGE}&order_by=updated_at&sort=desc"
            );
            write_fixture(
                out_root,
                &req_get(&url),
                &json_response(&Value::Array(items.clone())),
            )?;
            count += 1;
        }

        // Discussions, grouped by (proj, iid).
        let discussions = load_latest_by_key(&self.api_dir, ENTITY_DISCUSSION, |r| {
            let proj = r
                .get("project_full_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let iid = r.get("mr_iid").and_then(|v| v.as_u64()).unwrap_or(0);
            let id = r
                .get("discussion_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            discussion_pk_recipe(proj, iid as u32, id)
        })?;
        let mut disc_by_mr: BTreeMap<(String, u64), Vec<Value>> = BTreeMap::new();
        for rec in discussions.into_values() {
            let Some(proj) = rec.get("project_full_path").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(iid) = rec.get("mr_iid").and_then(|v| v.as_u64()) else {
                continue;
            };
            if let Some(raw) = rec.get("raw").cloned() {
                disc_by_mr
                    .entry((proj.to_string(), iid))
                    .or_default()
                    .push(raw);
            }
        }

        for (key, raw) in &mr_by_key {
            let (proj, iid) = key;
            let pid = urlencoding::encode(proj);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/projects/{pid}/merge_requests/{iid}")),
                &json_response(raw),
            )?;
            count += 1;

            let empty: Vec<Value> = Vec::new();
            let disc = disc_by_mr.get(key).unwrap_or(&empty);
            let url = format!(
                "{BASE}/projects/{pid}/merge_requests/{iid}/discussions?per_page={PER_PAGE}"
            );
            write_fixture(
                out_root,
                &req_get(&url),
                &json_response(&Value::Array(disc.clone())),
            )?;
            count += 1;
        }

        Ok(SynthesizeReport {
            fixtures_written: count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankweiler_etl::event_store::{diff_and_save, make_record};
    use frankweiler_etl::http::{fixture_key, HttpResponse};
    use serde_json::{json, Map};
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    fn write_event(api: &Path, entity: &str, key: Map<String, Value>, raw: Value) {
        let rec = make_record(key, raw);
        diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
    }

    #[test]
    fn emits_user_search_and_per_mr_fixtures() {
        let d = tempdir().unwrap();
        let api = d.path().join("gitlab_api");
        fs::create_dir_all(&api).unwrap();

        let mut k = Map::new();
        k.insert("user_id".into(), json!(7));
        write_event(&api, ENTITY_SELF, k, json!({"id": 7, "username": "tt"}));

        let proj = "ns/proj";
        let iid: u64 = 12;
        let mut k = Map::new();
        k.insert("project_full_path".into(), json!(proj));
        k.insert("mr_iid".into(), json!(iid));
        write_event(
            &api,
            ENTITY_MR,
            k,
            json!({
                "iid": iid,
                "web_url": format!("https://gitlab.com/{proj}/-/merge_requests/{iid}"),
                "state": "opened",
            }),
        );

        let mut k = Map::new();
        k.insert("project_full_path".into(), json!(proj));
        k.insert("mr_iid".into(), json!(iid));
        k.insert("discussion_id".into(), json!("abc"));
        write_event(
            &api,
            ENTITY_DISCUSSION,
            k,
            json!({"id": "abc", "notes": []}),
        );

        let out = d.path().join("playback");
        let report = GitlabSynth::new(&api).synthesize(&out).unwrap();
        // 1 user + 3 scopes + 1 MR detail + 1 discussions = 6
        assert_eq!(report.fixtures_written, 6);

        let req = req_get(&format!("{BASE}/user"));
        let p = out.join("gitlab").join(fixture_key(&req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["id"], 7);

        // reviewer scope uses reviewer_id={user_id}
        let url = format!(
            "{BASE}/merge_requests?reviewer_id=7&state=all&per_page={PER_PAGE}&order_by=updated_at&sort=desc"
        );
        let req = req_get(&url);
        let p = out.join("gitlab").join(fixture_key(&req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body[0]["iid"], iid);

        // detail fixture
        let pid = urlencoding::encode(proj);
        let req = req_get(&format!("{BASE}/projects/{pid}/merge_requests/{iid}"));
        let p = out.join("gitlab").join(fixture_key(&req));
        assert!(p.exists());
    }
}
