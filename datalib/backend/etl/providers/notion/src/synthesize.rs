//! Notion (official API) HTTP fixture synthesizer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use datalib_etl::event_store::load_latest_by_key;
use datalib_etl::http::HttpRequest;
use datalib_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

use crate::download::official::{BASE, PAGE_SIZE};
use crate::download::{
    ENTITY_ANCHOR_BLOCK, ENTITY_COMMENT, ENTITY_MARKDOWN, ENTITY_PAGE, ENTITY_USER,
};

pub struct NotionSynth {
    pub api_dir: PathBuf,
}

impl NotionSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

fn req_get(url: &str) -> HttpRequest {
    HttpRequest::get("notion", url).header("Accept", "application/json")
}

fn key_id(r: &Value) -> String {
    r.get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

impl Synthesizer for NotionSynth {
    fn name(&self) -> &'static str {
        "notion"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        if !self.api_dir.is_dir() {
            return Ok(SynthesizeReport::default());
        }
        let mut count = 0usize;

        let pages = load_latest_by_key(&self.api_dir, ENTITY_PAGE, key_id)?;
        let bodies = load_latest_by_key(&self.api_dir, ENTITY_MARKDOWN, key_id)?;
        let comments = load_latest_by_key(&self.api_dir, ENTITY_COMMENT, key_id)?;
        let users = load_latest_by_key(&self.api_dir, ENTITY_USER, key_id)?;
        let anchors = load_latest_by_key(&self.api_dir, ENTITY_ANCHOR_BLOCK, key_id)?;

        // /pages/{id}
        let mut page_ids: Vec<String> = Vec::new();
        for (id, rec) in &pages {
            if id.is_empty() {
                continue;
            }
            page_ids.push(id.clone());
            let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/pages/{id}")),
                &json_response(&raw),
            )?;
            count += 1;
        }
        page_ids.sort();

        // /pages/{id}/markdown — one per page. A page with no recorded
        // body still gets a fixture with empty markdown, which is the
        // common shape for a database row.
        let body_by_page: BTreeMap<String, Value> = bodies
            .iter()
            .filter_map(|(id, rec)| rec.get("raw").map(|r| (id.clone(), r.clone())))
            .collect();
        for pid in &page_ids {
            let body = body_by_page.get(pid).cloned().unwrap_or_else(|| {
                json!({
                    "object": "page_markdown",
                    "id": pid,
                    "markdown": "",
                    "truncated": false,
                    "unresolved_block_ids": [],
                })
            });
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/pages/{pid}/markdown")),
                &json_response(&body),
            )?;
            count += 1;
        }

        // Comments per page.
        let mut comments_by_page: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for pid in &page_ids {
            comments_by_page.entry(pid.clone()).or_default();
        }
        for (_, rec) in &comments {
            let Some(page_id) = rec.get("page_id").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(raw) = rec.get("raw").cloned() {
                comments_by_page
                    .entry(page_id.to_string())
                    .or_default()
                    .push(raw);
            }
        }
        for (pid, items) in &comments_by_page {
            let url = format!("{BASE}/comments?block_id={pid}&page_size={PAGE_SIZE}");
            let body = json!({
                "object": "list",
                "results": items,
                "has_more": false,
                "next_cursor": Value::Null,
            });
            write_fixture(out_root, &req_get(&url), &json_response(&body))?;
            count += 1;
        }

        // /v1/users/{id} — resolved one at a time, because
        // `GET /v1/users` is unavailable to personal access tokens.
        for (id, rec) in &users {
            if id.is_empty() {
                continue;
            }
            let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/users/{id}")),
                &json_response(&raw),
            )?;
            count += 1;
        }

        // /v1/blocks/{id} — only for blocks a comment hangs off, which
        // is the sole reason this provider reads a block at all.
        for (id, rec) in &anchors {
            if id.is_empty() {
                continue;
            }
            let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/blocks/{id}")),
                &json_response(&raw),
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
    use datalib_etl::event_store::{diff_and_save, make_record};
    use datalib_etl::http::{fixture_key, HttpResponse};
    use serde_json::Map;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    fn write_event(api: &Path, entity: &str, key: Map<String, Value>, raw: Value) {
        let rec = make_record(key, raw);
        diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
    }

    #[test]
    fn emits_page_markdown_and_comment_fixtures() {
        let d = tempdir().unwrap();
        let api = d.path().join("notion_api");
        fs::create_dir_all(&api).unwrap();

        let pid = "p1";
        let mut k = Map::new();
        k.insert("id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_PAGE,
            k,
            json!({"id": pid, "object": "page", "parent": {"type": "workspace"}}),
        );

        let mut k = Map::new();
        k.insert("id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_MARKDOWN,
            k,
            json!({"object": "page_markdown", "id": pid, "markdown": "# Hi\n", "truncated": false}),
        );

        let mut k = Map::new();
        k.insert("id".into(), json!("c1"));
        k.insert("page_id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_COMMENT,
            k,
            json!({"id": "c1", "object": "comment", "discussion_id": "d1"}),
        );

        let out = d.path().join("fixtures");
        let report = NotionSynth::new(&api).synthesize(&out).unwrap();
        assert_eq!(report.fixtures_written, 3, "page + markdown + comments");

        for url in [
            format!("{BASE}/pages/{pid}"),
            format!("{BASE}/pages/{pid}/markdown"),
            format!("{BASE}/comments?block_id={pid}&page_size={PAGE_SIZE}"),
        ] {
            let path = out.join("notion").join(fixture_key(&req_get(&url)));
            assert!(path.exists(), "missing fixture for {url}");
        }
    }

    /// A page with no stored body still needs a markdown fixture —
    /// otherwise playback 404s on the majority of a real workspace,
    /// where most pages are database rows with no body at all.
    #[test]
    fn a_page_with_no_body_still_gets_an_empty_markdown_fixture() {
        let d = tempdir().unwrap();
        let api = d.path().join("notion_api");
        fs::create_dir_all(&api).unwrap();
        let pid = "row1";
        let mut k = Map::new();
        k.insert("id".into(), json!(pid));
        write_event(&api, ENTITY_PAGE, k, json!({"id": pid, "object": "page"}));

        let out = d.path().join("fixtures");
        NotionSynth::new(&api).synthesize(&out).unwrap();
        let path = out.join("notion").join(fixture_key(&req_get(&format!(
            "{BASE}/pages/{pid}/markdown"
        ))));
        assert!(path.exists(), "body-less page must still have a fixture");
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["markdown"], "");
    }
}
