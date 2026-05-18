//! GitHub HTTP fixture synthesizer.
//!
//! Walks the event-store layout the live downloader writes under
//! `<api_dir>/<entity>/{created,updated}/events.jsonl` and emits playback
//! fixtures for every request [`crate::extract`] would issue:
//!
//! * `GET https://api.github.com/user` — viewer identity, from the latest
//!   `self_identity` record's `raw`.
//! * `GET /search/issues?q=is:pr {scope}&per_page=100&sort=updated&order=desc`
//!   — one fixture per [`crate::extract::DEFAULT_SCOPES`] scope.
//!   We assume **first-run / full-sync playback**: no `updated:>=since`
//!   clause is appended. The body is a `{"items": [...]}` envelope with
//!   one minimal item per known PR (`repository_url` + `number` are the
//!   only fields extract reads). No `Link: rel="next"` header → paginate
//!   stops after one page.
//! * `GET /repos/{repo}/pulls/{num}` — the per-PR detail, from the latest
//!   `pull_request` record's `raw`.
//! * `GET /repos/{repo}/issues/{num}/comments?per_page=100` — array of
//!   `issue_comment.raw` for that PR.
//! * `GET /repos/{repo}/pulls/{num}/reviews?per_page=100` — array of
//!   `pr_review.raw` for that PR.
//! * `GET /repos/{repo}/pulls/{num}/comments?per_page=100` — array of
//!   `pr_review_comment.raw` for that PR.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::event_store::load_latest_by_key;
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

use crate::extract::{
    BASE, DEFAULT_SCOPES, ENTITY_ISSUE_COMMENT, ENTITY_PR, ENTITY_PR_REVIEW,
    ENTITY_PR_REVIEW_COMMENT, ENTITY_SELF, PER_PAGE,
};

pub struct GithubSynth {
    pub api_dir: PathBuf,
}

impl GithubSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("github", url)
}

fn pr_repo_num(rec: &Value) -> Option<(String, u64)> {
    let repo = rec.get("repo_full_name")?.as_str()?.to_string();
    let num = rec.get("pr_number")?.as_u64()?;
    Some((repo, num))
}

impl Synthesizer for GithubSynth {
    fn name(&self) -> &'static str {
        "github"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        if !self.api_dir.is_dir() {
            return Ok(SynthesizeReport::default());
        }
        let mut count = 0usize;

        // /user from latest self_identity.
        let selves = load_latest_by_key(&self.api_dir, ENTITY_SELF, |r| {
            r.get("user_id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
                .unwrap_or_default()
        })?;
        if let Some(latest) = selves.values().next() {
            let raw = latest.get("raw").cloned().unwrap_or(Value::Null);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/user")),
                &json_response(&raw),
            )?;
            count += 1;
        }

        // PRs, grouped by (repo, num).
        let prs = load_latest_by_key(&self.api_dir, ENTITY_PR, |r| {
            pr_repo_num(r)
                .map(|(r, n)| format!("{r}#{n}"))
                .unwrap_or_default()
        })?;
        let mut pr_by_key: BTreeMap<(String, u64), Value> = BTreeMap::new();
        for rec in prs.into_values() {
            if let Some(k) = pr_repo_num(&rec) {
                if let Some(raw) = rec.get("raw").cloned() {
                    pr_by_key.insert(k, raw);
                }
            }
        }

        // Search fixtures per default scope — minimal item shape: extract
        // only reads `repository_url` (to derive repo) and `number`.
        let items: Vec<Value> = pr_by_key
            .keys()
            .map(|(repo, num)| {
                json!({
                    "repository_url": format!("{BASE}/repos/{repo}"),
                    "number": num,
                })
            })
            .collect();
        for scope in DEFAULT_SCOPES {
            let q = format!("is:pr {scope}");
            let url = format!(
                "{BASE}/search/issues?q={}&per_page={PER_PAGE}&sort=updated&order=desc",
                urlencoding::encode(&q)
            );
            let body = json!({
                "total_count": items.len(),
                "incomplete_results": false,
                "items": items,
            });
            write_fixture(out_root, &req_get(&url), &json_response(&body))?;
            count += 1;
        }

        // Group child entities by (repo, num).
        let group_by_pr = |entity: &str| -> Result<BTreeMap<(String, u64), Vec<Value>>> {
            let recs = load_latest_by_key(&self.api_dir, entity, |r| {
                let repo = r
                    .get("repo_full_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let num = r.get("pr_number").and_then(|v| v.as_u64()).unwrap_or(0);
                let id = r
                    .get("comment_id")
                    .or_else(|| r.get("review_id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                format!("{repo}#{num}#{id}")
            })?;
            let mut grouped: BTreeMap<(String, u64), Vec<Value>> = BTreeMap::new();
            for rec in recs.into_values() {
                let Some(repo) = rec.get("repo_full_name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(num) = rec.get("pr_number").and_then(|v| v.as_u64()) else {
                    continue;
                };
                if let Some(raw) = rec.get("raw").cloned() {
                    grouped
                        .entry((repo.to_string(), num))
                        .or_default()
                        .push(raw);
                }
            }
            Ok(grouped)
        };

        let issue_comments = group_by_pr(ENTITY_ISSUE_COMMENT)?;
        let reviews = group_by_pr(ENTITY_PR_REVIEW)?;
        let review_comments = group_by_pr(ENTITY_PR_REVIEW_COMMENT)?;

        // Per-PR detail + the three list endpoints (always emit, even if
        // the list is empty, so extract's paginate gets a defined answer).
        for (key, raw) in &pr_by_key {
            let (repo, num) = key;
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/repos/{repo}/pulls/{num}")),
                &json_response(raw),
            )?;
            count += 1;

            let empty: Vec<Value> = Vec::new();
            for (endpoint, src) in [
                (
                    format!("{BASE}/repos/{repo}/issues/{num}/comments?per_page={PER_PAGE}"),
                    issue_comments.get(key).unwrap_or(&empty),
                ),
                (
                    format!("{BASE}/repos/{repo}/pulls/{num}/reviews?per_page={PER_PAGE}"),
                    reviews.get(key).unwrap_or(&empty),
                ),
                (
                    format!("{BASE}/repos/{repo}/pulls/{num}/comments?per_page={PER_PAGE}"),
                    review_comments.get(key).unwrap_or(&empty),
                ),
            ] {
                write_fixture(
                    out_root,
                    &req_get(&endpoint),
                    &json_response(&Value::Array(src.clone())),
                )?;
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
    use frankweiler_etl::event_store::{diff_and_save, make_record};
    use frankweiler_etl::http::{fixture_key, HttpResponse};
    use serde_json::Map;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    fn write_event(api: &Path, entity: &str, key: Map<String, Value>, raw: Value) {
        let rec = make_record(key, raw);
        diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
    }

    #[test]
    fn emits_user_search_and_per_pr_fixtures() {
        let d = tempdir().unwrap();
        let api = d.path().join("github_api");
        fs::create_dir_all(&api).unwrap();

        // self_identity
        let mut k = Map::new();
        k.insert("user_id".into(), json!(42));
        write_event(&api, ENTITY_SELF, k, json!({"id": 42, "login": "octocat"}));

        // one PR
        let repo = "octocat/hello";
        let num = 7u64;
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(repo));
        k.insert("pr_number".into(), json!(num));
        write_event(
            &api,
            ENTITY_PR,
            k,
            json!({"number": num, "title": "T", "state": "open"}),
        );

        // one issue comment for that PR
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(repo));
        k.insert("pr_number".into(), json!(num));
        k.insert("comment_id".into(), json!(101));
        write_event(
            &api,
            ENTITY_ISSUE_COMMENT,
            k,
            json!({"id": 101, "body": "hi"}),
        );

        let out = d.path().join("playback");
        let report = GithubSynth::new(&api).synthesize(&out).unwrap();
        // 1 user + 3 scope searches + 1 PR detail + 3 list endpoints = 8
        assert_eq!(report.fixtures_written, 8);

        // /user
        let req = req_get(&format!("{BASE}/user"));
        let p = out.join("github").join(fixture_key(&req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["login"], "octocat");

        // search fixture contains our PR
        let q = format!("is:pr {}", DEFAULT_SCOPES[0]);
        let url = format!(
            "{BASE}/search/issues?q={}&per_page={PER_PAGE}&sort=updated&order=desc",
            urlencoding::encode(&q)
        );
        let req = req_get(&url);
        let p = out.join("github").join(fixture_key(&req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["items"][0]["number"], 7);
        assert_eq!(
            body["items"][0]["repository_url"],
            format!("{BASE}/repos/{repo}")
        );

        // issue comments list
        let url = format!("{BASE}/repos/{repo}/issues/{num}/comments?per_page={PER_PAGE}");
        let req = req_get(&url);
        let p = out.join("github").join(fixture_key(&req));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body[0]["id"], 101);
    }
}
