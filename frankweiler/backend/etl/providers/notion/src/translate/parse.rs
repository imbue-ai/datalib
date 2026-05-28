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
#[cfg(test)]
use crate::extract::db::BlockUpsert;

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
        db.upsert_blocks(&[BlockUpsert {
            id: "b1".into(),
            parent_id: Some("p1".into()),
            page_id: Some("p1".into()),
            page_order: Some(0),
            last_edited_time: None,
            payload: Some(serde_json::to_string(&json!({"id": "b1", "type": "paragraph"})).unwrap()),
        }])
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
    async fn block_load_preserves_page_order() {
        // BFS discovery order has to survive a round-trip even when
        // block ids sort differently. The old JSONL implementation
        // keyed by uuid in a BTreeMap and lost order — render came
        // out scrambled. We now persist `page_order` and `ORDER BY
        // (page_id, page_order)` in load_blocks.
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        // ids whose lex order is "aaaa-2" < "mmmm-3" < "zzzz-1" but
        // whose discovery order is the reverse — proves page_order
        // wins over lex(id).
        let inputs = [("zzzz-1", 0_i64), ("aaaa-2", 1), ("mmmm-3", 2)];
        for (id, order) in &inputs {
            db.upsert_blocks(&[BlockUpsert {
                id: (*id).into(),
                parent_id: None,
                page_id: Some("p1".into()),
                page_order: Some(*order),
                last_edited_time: None,
                payload: Some(serde_json::to_string(&json!({"id": id})).unwrap()),
            }])
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
