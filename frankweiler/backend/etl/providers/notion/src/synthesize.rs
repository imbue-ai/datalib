//! Notion (official API) HTTP fixture synthesizer.
//!
//! Reads the **checked-in JSONL fixture tree** under
//! `<api_dir>/notion_official_{page,block,comment}/{created,updated}/events.jsonl`
//! and emits HTTP playback fixtures for every request
//! [`crate::download::official::NotionOfficialClient`] would issue:
//!
//! * `GET /v1/pages/{id}` — one per page record, body = `raw`.
//! * `GET /v1/blocks/{parent_id}/children?page_size=100` — one per block
//!   parent (= each page id plus every block with children). We collapse
//!   the cursor chain into a single page (`has_more=false`); download only
//!   walks `start_cursor` URLs when `has_more=true`, so no cursor variant
//!   is ever requested.
//! * `GET /v1/comments?block_id={page_id}&page_size=100` — one per page,
//!   array of that page's `comment.raw` records.
//!
//! Why JSONL and not the runtime doltlite DB? The synth step is a test
//! helper: it converts checked-in source fixtures into HTTP playback
//! responses. Source fixtures stay in JSONL because that's the format
//! humans diff and edit; the runtime doltlite database is produced
//! naturally by download running against the playback fixtures this
//! synth writes. Keeping JSONL as the synth input means we don't
//! checked-in a binary sqlite file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::event_store::load_latest_by_key;
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

use crate::download::official::{BASE, PAGE_SIZE};
use crate::download::{ENTITY_BLOCK, ENTITY_COMMENT, ENTITY_PAGE};

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

fn parent_id_of(block_raw: &Value, fallback_page_id: &str) -> String {
    let parent = block_raw.get("parent");
    if let Some(p) = parent {
        if let Some(bid) = p.get("block_id").and_then(|v| v.as_str()) {
            return bid.to_string();
        }
        if let Some(pid) = p.get("page_id").and_then(|v| v.as_str()) {
            return pid.to_string();
        }
    }
    fallback_page_id.to_string()
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
        let blocks = load_latest_by_key(&self.api_dir, ENTITY_BLOCK, key_id)?;
        let comments = load_latest_by_key(&self.api_dir, ENTITY_COMMENT, key_id)?;

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

        // Group blocks by parent id. Seed every page id so each page gets
        // its own children fixture even when it has no recorded blocks.
        let mut children_by_parent: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for pid in &page_ids {
            children_by_parent.entry(pid.clone()).or_default();
        }
        for rec in blocks.values() {
            let raw = match rec.get("raw") {
                Some(r) => r.clone(),
                None => continue,
            };
            let page_id = rec.get("page_id").and_then(|v| v.as_str()).unwrap_or("");
            let parent = parent_id_of(&raw, page_id);
            children_by_parent.entry(parent).or_default().push(raw);
        }

        for (parent, children) in &children_by_parent {
            let url = format!("{BASE}/blocks/{parent}/children?page_size={PAGE_SIZE}");
            let body = json!({
                "object": "list",
                "results": children,
                "has_more": false,
                "next_cursor": Value::Null,
            });
            write_fixture(out_root, &req_get(&url), &json_response(&body))?;
            count += 1;
        }

        // Comments per page.
        let mut comments_by_page: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for pid in &page_ids {
            comments_by_page.entry(pid.clone()).or_default();
        }
        for rec in comments.values() {
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
    fn emits_pages_blocks_and_comments() {
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

        // one direct child block of the page
        let mut k = Map::new();
        k.insert("id".into(), json!("b1"));
        k.insert("page_id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_BLOCK,
            k,
            json!({
                "id": "b1", "type": "paragraph", "has_children": false,
                "parent": {"type": "page_id", "page_id": pid},
            }),
        );
        // one nested block under b1
        let mut k = Map::new();
        k.insert("id".into(), json!("b2"));
        k.insert("page_id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_BLOCK,
            k,
            json!({
                "id": "b2", "type": "paragraph", "has_children": false,
                "parent": {"type": "block_id", "block_id": "b1"},
            }),
        );

        // one comment on the page
        let mut k = Map::new();
        k.insert("id".into(), json!("c1"));
        k.insert("page_id".into(), json!(pid));
        write_event(
            &api,
            ENTITY_COMMENT,
            k,
            json!({"id": "c1", "rich_text": []}),
        );

        let out = d.path().join("playback");
        let report = NotionSynth::new(&api).synthesize(&out).unwrap();
        // 1 page + (children for p1 and b1 = 2) + 1 comments = 4
        assert_eq!(report.fixtures_written, 4);

        // page fixture
        let req = req_get(&format!("{BASE}/pages/{pid}"));
        let p = out.join("notion").join(fixture_key(&req));
        assert!(p.exists());

        // children of page contains b1
        let url = format!("{BASE}/blocks/{pid}/children?page_size={PAGE_SIZE}");
        let p = out.join("notion").join(fixture_key(&req_get(&url)));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["results"][0]["id"], "b1");
        assert_eq!(body["has_more"], false);

        // children of b1 contains b2
        let url = format!("{BASE}/blocks/b1/children?page_size={PAGE_SIZE}");
        let p = out.join("notion").join(fixture_key(&req_get(&url)));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["results"][0]["id"], "b2");

        // comments
        let url = format!("{BASE}/comments?block_id={pid}&page_size={PAGE_SIZE}");
        let p = out.join("notion").join(fixture_key(&req_get(&url)));
        let resp: HttpResponse = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["results"][0]["id"], "c1");
    }
}
