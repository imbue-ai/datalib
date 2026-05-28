//! Read raw Notion payloads from the doltlite database written by
//! [`crate::extract`].
//!
//! Accepts either a path to the `.doltlite_db` file directly or the
//! resolved-input-path of the source (e.g. `<data_root>/raw/notion-api`),
//! which we rewrite to the sibling `.doltlite_db` file. This keeps the
//! sync orchestrator's `resolved_input_path` contract unchanged.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::extract::db::{block_on_load_all, db_path_for, BlobBytes, LoadedRaw};

pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_BLOCK: &str = "notion_official_block";
pub const ENTITY_COMMENT: &str = "notion_official_comment";

#[derive(Debug, Default, Clone)]
pub struct ParsedNotionOfficial {
    pub pages: Vec<Value>,
    /// Blocks in BFS / insertion order, matching how the downloader
    /// discovered them (extract/mod.rs::walk_page_blocks). The render
    /// step relies on this order for section / toggle layout.
    pub blocks: Vec<Value>,
    pub comments: Vec<Value>,
    pub user_names: HashMap<String, String>,
    pub media_urls: HashMap<String, String>,
    pub bookmark_titles: HashMap<String, String>,
    /// Blob bytes keyed by owning block id. Render writes these to disk
    /// next to the rendered markdown and rewrites image links to point
    /// at the local copy.
    pub blobs_by_owner: HashMap<String, BlobBytes>,
}

/// Read raw payloads out of the doltlite DB. The `page_id` column of
/// each block/comment is injected back into the JSON value under the
/// `page_id` key so downstream consumers that grew up on the JSONL
/// shape — where this field rode alongside `raw` — keep working.
pub fn parse_api_dir(path: &Path) -> Result<ParsedNotionOfficial> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        return Ok(ParsedNotionOfficial::default());
    }
    let LoadedRaw {
        pages,
        blocks,
        comments,
        blobs_by_owner,
    } = block_on_load_all(&db_path)?;

    // Inject page_id into block/comment values so existing readers
    // (notion/translate/render.rs, synthesize) that expect the wrapper
    // shape don't need a parallel API.
    let blocks_v: Vec<Value> = blocks
        .into_iter()
        .map(|(mut v, pid)| {
            if let (Some(obj), Some(pid)) = (v.as_object_mut(), pid) {
                obj.entry("page_id").or_insert(Value::String(pid));
            }
            v
        })
        .collect();
    let comments_v: Vec<Value> = comments
        .into_iter()
        .map(|(mut v, pid)| {
            if let (Some(obj), Some(pid)) = (v.as_object_mut(), pid) {
                obj.entry("page_id").or_insert(Value::String(pid));
            }
            v
        })
        .collect();

    Ok(ParsedNotionOfficial {
        pages,
        blocks: blocks_v,
        comments: comments_v,
        user_names: HashMap::new(),
        media_urls: HashMap::new(),
        bookmark_titles: HashMap::new(),
        blobs_by_owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::RawDb;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parse_round_trips_pages_and_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            None,
            Some("2026-05-21T19:37:00Z".into()),
            Some(serde_json::to_string(&json!({"id": "p1", "object": "page"})).unwrap()),
        )])
        .await
        .unwrap();
        db.upsert_blocks(&[(
            "b1".into(),
            Some("p1".into()),
            Some("p1".into()),
            None,
            Some(serde_json::to_string(&json!({"id": "b1", "type": "paragraph"})).unwrap()),
        )])
        .await
        .unwrap();
        drop(db);

        let parsed = parse_api_dir(&db_file).unwrap();
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.pages[0]["id"], "p1");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0]["page_id"], "p1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_load_preserves_insertion_order() {
        // Upstream merge fix: when the JSONL reader used a BTreeMap
        // keyed on uuid, blocks came back in lex-of-uuid order and
        // sections rendered scrambled. The DB load path must return
        // blocks in insertion order (= BFS discovery order) so the
        // render keeps page layout intact.
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        let ids = ["zzzz-1", "aaaa-2", "mmmm-3"];
        for id in &ids {
            db.upsert_blocks(&[(
                id.to_string(),
                None,
                Some("p1".into()),
                None,
                Some(serde_json::to_string(&json!({"id": id})).unwrap()),
            )])
            .await
            .unwrap();
        }
        drop(db);
        let parsed = parse_api_dir(&db_file).unwrap();
        let got: Vec<&str> = parsed
            .blocks
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["zzzz-1", "aaaa-2", "mmmm-3"]);
    }
}
