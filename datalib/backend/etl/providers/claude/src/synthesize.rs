//! Claude (claude.ai) HTTP fixture synthesizer.
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
//! * `GET /organizations/{org}/projects` and
//!   `…/projects/{project}/docs` — read from `<api_dir>/projects/*.json`,
//!   each of which holds one project with its knowledge documents
//!   nested under `docs`. The listing fixture is written for **every**
//!   org even when it is empty, because the downloader lists projects
//!   per org unconditionally and a missing fixture is a playback error,
//!   not an empty result.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::http::HttpRequest;
use datalib_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

const BASE: &str = "https://claude.ai/api";
const DETAIL_QUERY: &str =
    "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong";

pub struct ClaudeSynth {
    pub api_dir: PathBuf,
}

impl ClaudeSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("claude", url).header("Accept", "application/json")
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

/// Display name for the org a conversation or project belongs to,
/// read from the same `_source` block as the uuid.
/// [`crate::download::normalize::normalize_to_export_shape`] stamps
/// `org_name` alongside `org_uuid` on everything the API downloader
/// writes, so a real captured snapshot carries it; a bulk export
/// (which has no org scope at all) does not.
fn org_name_of(v: &Value) -> Option<String> {
    v.get("_source")
        .and_then(|s| s.get("org_name"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Which org a project belongs to. Same `_source.org_uuid` convention
/// the conversation fixtures use; `None` when the file predates it (a
/// real bulk export has no org scope), in which case the caller files
/// the project under the first org.
fn project_org_uuid(project: &Value) -> Option<String> {
    project
        .get("_source")
        .and_then(|s| s.get("org_uuid"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Read `<api_dir>/projects/*.json`, sorted by filename so the emitted
/// listing order is deterministic.
fn read_projects(api_dir: &Path) -> Result<Vec<Value>> {
    let dir = api_dir.join("projects");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let v: Value = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parse {}", path.display()))?;
        out.push(v);
    }
    Ok(out)
}

/// The project as the *listing* endpoint returns it: everything except
/// the nested `docs`, which the live API serves from its own endpoint.
fn project_listing_item(project: &Value) -> Value {
    let mut obj = project.as_object().cloned().unwrap_or_default();
    obj.remove("docs");
    Value::Object(obj)
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

impl Synthesizer for ClaudeSynth {
    fn name(&self) -> &'static str {
        "claude"
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

        let projects = read_projects(&self.api_dir)?;

        let mut by_org: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        // Display names, keyed by org uuid. Collected from both
        // conversations and projects because either kind of file can
        // be the only one that mentions a given org.
        let mut org_names: BTreeMap<String, String> = BTreeMap::new();
        for c in &convs {
            if let Some(org) = org_uuid_of(c) {
                if let Some(name) = org_name_of(c) {
                    org_names.entry(org.clone()).or_insert(name);
                }
                by_org.entry(org).or_default().push(c);
            }
        }
        // A project can live in an org that has no conversations, and
        // that org still has to appear in /organizations or the
        // downloader will never ask about it.
        for p in &projects {
            if let Some(org) = project_org_uuid(p) {
                if let Some(name) = org_name_of(p) {
                    org_names.entry(org.clone()).or_insert(name);
                }
                by_org.entry(org).or_default();
            }
        }

        // Projects without an explicit `_source.org_uuid` (the shape a
        // real bulk export has) go to the first org, deterministically.
        let default_org = by_org.keys().next().cloned();
        let mut projects_by_org: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for org in by_org.keys() {
            projects_by_org.insert(org.clone(), Vec::new());
        }
        for p in &projects {
            let Some(org) = project_org_uuid(p).or_else(|| default_org.clone()) else {
                continue;
            };
            projects_by_org.entry(org).or_default().push(p);
        }

        let mut count = 0usize;

        // /organizations
        //
        // The `name` emitted here is what the downloader stores as the
        // `org_name` column (see `download::org_identity`), which
        // becomes `grid_rows.org_name`. Emitting the uuid as the name —
        // as this did originally — makes `org_name` and `org_uuid`
        // indistinguishable everywhere downstream, so a fixture-backed
        // test cannot tell the two columns apart and a transposed
        // binding reads as correct. Fall back to the uuid only when the
        // snapshot genuinely carries no name.
        let orgs: Vec<Value> = by_org
            .keys()
            .map(|uuid| {
                let name = org_names.get(uuid).unwrap_or(uuid);
                json!({"uuid": uuid, "name": name})
            })
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

            // Project listing — written even when empty (see module doc).
            let org_projects = projects_by_org.get(org).cloned().unwrap_or_default();
            let project_listing: Vec<Value> = org_projects
                .iter()
                .map(|p| project_listing_item(p))
                .collect();
            let req = req_get(&format!("{BASE}/organizations/{org}/projects"));
            write_fixture(
                out_root,
                &req,
                &json_response(&Value::Array(project_listing)),
            )?;
            count += 1;

            // Knowledge docs per project.
            for p in &org_projects {
                let Some(uuid) = p.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                let docs = p.get("docs").cloned().unwrap_or_else(|| json!([]));
                let req = req_get(&format!("{BASE}/organizations/{org}/projects/{uuid}/docs"));
                write_fixture(out_root, &req, &json_response(&docs))?;
                count += 1;
            }

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
    use datalib_etl::http::{fixture_key, HttpResponse};
    use tempfile::tempdir;

    #[test]
    fn emits_orgs_listings_and_details() {
        let d = tempdir().unwrap();
        let api = d.path().join("claude_export");
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
        let report = ClaudeSynth::new(&api).synthesize(&out).unwrap();
        // /organizations + 2 chat listings + 3 detail + 2 project
        // listings (one per org, empty here) = 8
        assert_eq!(report.fixtures_written, 8);

        let orgs_req = req_get(&format!("{BASE}/organizations"));
        let p = out.join("claude").join(fixture_key(&orgs_req));
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
        let p = out.join("claude").join(fixture_key(&detail_req));
        assert!(p.exists(), "missing detail fixture at {}", p.display());

        // The downloader lists projects for every org unconditionally,
        // so an org with no projects still needs an (empty) fixture —
        // otherwise playback errors instead of returning nothing.
        for org in ["org-a", "org-b"] {
            let req = req_get(&format!("{BASE}/organizations/{org}/projects"));
            let p = out.join("claude").join(fixture_key(&req));
            assert!(p.exists(), "missing project listing for {org}");
            let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            let body: Value = serde_json::from_slice(&resp.body).unwrap();
            assert_eq!(body.as_array().map(Vec::len), Some(0));
        }
    }

    /// A project file's `docs` are split off into the separate endpoint
    /// the live API serves them from, and do not leak into the listing.
    #[test]
    fn splits_project_docs_out_of_the_listing() {
        let d = tempdir().unwrap();
        let api = d.path().join("claude_export");
        fs::create_dir_all(api.join("projects")).unwrap();
        fs::write(
            api.join("conversations.json"),
            serde_json::to_vec(&json!([{
                "uuid": "c1", "organization_uuid": "org-a", "chat_messages": []
            }]))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            api.join("projects").join("p1.json"),
            serde_json::to_vec(&json!({
                "uuid": "p1",
                "name": "Bridge Ops",
                "_source": {"org_uuid": "org-a"},
                "docs": [{"uuid": "d1", "file_name": "a.md", "content": "hello"}]
            }))
            .unwrap(),
        )
        .unwrap();

        let out = d.path().join("playback");
        ClaudeSynth::new(&api).synthesize(&out).unwrap();

        let listing_req = req_get(&format!("{BASE}/organizations/org-a/projects"));
        let p = out.join("claude").join(fixture_key(&listing_req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        let first = &body.as_array().unwrap()[0];
        assert_eq!(first.get("name").unwrap(), "Bridge Ops");
        assert!(
            first.get("docs").is_none(),
            "docs must not ride along in the listing: {first}"
        );

        let docs_req = req_get(&format!("{BASE}/organizations/org-a/projects/p1/docs"));
        let p = out.join("claude").join(fixture_key(&docs_req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        let docs = body.as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].get("content").unwrap(), "hello");
    }
}
