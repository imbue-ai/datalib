//! Notion (official API) HTTP fixture synthesizer.
//!
//! Walks the event-store JSONL the live downloader writes under
//! `<api_dir>/notion_official_{page,block,comment}/{created,updated}/events.jsonl`
//! and emits playback fixtures for every request
//! [`crate::extract::official::NotionOfficialClient`] would issue:
//!
//! * `GET /v1/pages/{id}` — one per page record, body = `raw`.
//! * `GET /v1/blocks/{parent_id}/children?page_size=100` — one per block
//!   parent (= each page id plus every block with children). We collapse
//!   the cursor chain into a single page (`has_more=false`); extract only
//!   walks `start_cursor` URLs when `has_more=true`, so no cursor variant
//!   is ever requested.
//! * `GET /v1/comments?block_id={page_id}&page_size=100` — one per page,
//!   array of that page's `comment.raw` records.
//!
//! Parent grouping uses each block's `raw.parent.{block_id,page_id}`,
//! falling back to `page_id` from the record key if missing — sufficient
//! for the synthesized BFS to reproduce extract's request stream.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::http::HttpRequest;
use frankweiler_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

use crate::extract::db::{block_on_load_all, db_path_for, LoadedRaw};
use crate::extract::official::{BASE, PAGE_SIZE};

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
        let db_path = db_path_for(&self.api_dir);
        if !db_path.exists() {
            return Ok(SynthesizeReport::default());
        }
        let LoadedRaw {
            pages,
            blocks,
            comments,
            ..
        } = block_on_load_all(&db_path)?;
        let mut count = 0usize;

        // /pages/{id}
        let mut page_ids: Vec<String> = Vec::new();
        for raw in &pages {
            let Some(id) = raw.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            page_ids.push(id.to_string());
            write_fixture(
                out_root,
                &req_get(&format!("{BASE}/pages/{id}")),
                &json_response(raw),
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
        for (raw, page_id_opt) in &blocks {
            let page_id = page_id_opt.as_deref().unwrap_or("");
            let parent = parent_id_of(raw, page_id);
            children_by_parent
                .entry(parent)
                .or_default()
                .push(raw.clone());
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
        for (raw, page_id_opt) in &comments {
            let Some(page_id) = page_id_opt.as_deref() else {
                continue;
            };
            comments_by_page
                .entry(page_id.to_string())
                .or_default()
                .push(raw.clone());
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
    use crate::extract::db::BlockUpsert;
    use crate::extract::RawDb;
    use frankweiler_etl::http::{fixture_key, HttpResponse};
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emits_pages_blocks_and_comments() {
        let d = tempdir().unwrap();
        let db_file = d.path().join("notion-api.doltlite_db");

        let db = RawDb::open(&db_file).await.unwrap();
        let pid = "p1";
        db.upsert_pages(&[(
            pid.into(),
            None,
            None,
            Some(
                serde_json::to_string(
                    &json!({"id": pid, "object": "page", "parent": {"type": "workspace"}}),
                )
                .unwrap(),
            ),
        )])
        .await
        .unwrap();
        db.upsert_blocks(&[
            BlockUpsert {
                id: "b1".into(),
                parent_id: Some(pid.into()),
                page_id: Some(pid.into()),
                page_order: Some(0),
                last_edited_time: None,
                payload: Some(
                    serde_json::to_string(&json!({
                        "id": "b1", "type": "paragraph", "has_children": false,
                        "parent": {"type": "page_id", "page_id": pid},
                    }))
                    .unwrap(),
                ),
            },
            BlockUpsert {
                id: "b2".into(),
                parent_id: Some("b1".into()),
                page_id: Some(pid.into()),
                page_order: Some(1),
                last_edited_time: None,
                payload: Some(
                    serde_json::to_string(&json!({
                        "id": "b2", "type": "paragraph", "has_children": false,
                        "parent": {"type": "block_id", "block_id": "b1"},
                    }))
                    .unwrap(),
                ),
            },
        ])
        .await
        .unwrap();
        db.upsert_comments(&[(
            "c1".into(),
            pid.into(),
            Some(pid.into()),
            serde_json::to_string(&json!({"id": "c1", "rich_text": []})).unwrap(),
        )])
        .await
        .unwrap();
        drop(db);

        let out = d.path().join("playback");
        let report = NotionSynth::new(&db_file).synthesize(&out).unwrap();
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
