//! Read raw Notion payloads from the doltlite database written by
//! [`datalib_etl_notion::download`].

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use datalib_etl_notion::download::db::{block_on_load_all, db_path_for, LoadedRaw};

#[derive(Clone, Default)]
pub struct ParsedNotion {
    pub pages: Vec<Value>,
    /// Page bodies as Notion rendered them, keyed by page id. Attachment
    /// URLs are already slots, not signed links.
    pub markdown_by_page: HashMap<String, String>,
    pub comments: Vec<Value>,
    /// `user_id -> display name`. Notion resolves comment authors on
    /// the comment itself, so this is for the places it does not: a
    /// page's `created_by` / `last_edited_by`, and people properties.
    pub user_names: HashMap<String, String>,
    /// `block_id -> the text a comment on that block is anchored to`.
    /// A comment names its block and carries no quote, so without this
    /// a thread's anchor is an opaque uuid.
    pub anchor_text: HashMap<String, String>,
    /// What the `dolt_diff` scan concluded, carried through so render
    /// can advance the cursor on success.
    pub scan: datalib_etl::doltlite_raw::DiffScan,
    /// Pages Notion no longer has, and discussions whose last comment
    /// went. Both are documents to remove, and they are separate
    /// removals: a page and its threads carry different
    /// `conversation_uuid`s.
    pub vanished_pages: Vec<String>,
    pub vanished_discussions: Vec<String>,
    /// Attachment bytes per page, pre-loaded from the sibling CAS.
    /// Render calls `bundle.materialize_to_dir(<page_dir>/blobs)` once
    /// per page and resolves each slot with `bundle.filename_for`.
    pub blobs_by_page: HashMap<String, datalib_etl::blob_cas::BlobBundle>,
}

/// Read the pages that changed since `last_render_hash`, and say which
/// documents went away.
///
/// The filtering is what makes render incremental: a steady-state run
/// gets an empty changed set and renders nothing. It narrows the
/// *render*, which is where the cost is — writing files, building
/// `grid_rows`, hashing — and not the read, which still walks the
/// store's rows once. Say so plainly; the scan is not a substitute for
/// a narrower query, and if the read ever dominates, that is the thing
/// to fix.
///
/// The `page_id` column of each comment is injected back into the JSON
/// value so downstream consumers can group without a second lookup.
pub fn parse_api_dir(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedNotion> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        return Ok(ParsedNotion::default());
    }
    let LoadedRaw {
        pages,
        page_markdown,
        comments,
        user_names,
        comment_anchors,
        blobs_by_page,
        scan,
        vanished_pages,
        vanished_discussions,
    } = block_on_load_all(&db_path, last_render_hash)?;

    let comments: Vec<Value> = comments
        .into_iter()
        .map(|(mut v, pid)| {
            if let (Some(obj), Some(pid)) = (v.as_object_mut(), pid) {
                obj.entry("page_id").or_insert(Value::String(pid));
            }
            v
        })
        .collect();

    Ok(ParsedNotion {
        pages,
        markdown_by_page: page_markdown.into_iter().collect(),
        comments,
        user_names,
        anchor_text: comment_anchors,
        blobs_by_page,
        scan,
        vanished_pages,
        vanished_discussions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_notion::download::db::{PageMarkdownUpsert, PageUpsert};
    use datalib_etl_notion::download::RawDb;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parse_round_trips_pages_and_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "p1".into(),
            last_edited_time: Some("2026-05-21T19:37:00Z".into()),
            payload: Some(serde_json::to_string(&json!({"id": "p1", "object": "page"})).unwrap()),
            ..Default::default()
        }])
        .await
        .unwrap();
        db.upsert_page_markdown(&[PageMarkdownUpsert {
            id: "p1".into(),
            markdown: "# Hello\n".into(),
            ..Default::default()
        }])
        .await
        .unwrap();
        // Sealed before render reads it, exactly as the download step does:
        // render pins HEAD, so an uncommitted row is invisible to it. Without
        // this the test asserts against the working set, which is the bug the
        // pinning work exists to remove.
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test fixture")
            .await
            .unwrap();
        // Closed, not dropped: `parse_api_dir` reopens this store.
        db.close().await;

        let parsed = parse_api_dir(&db_file, None).unwrap();
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.pages[0]["id"], "p1");
        assert_eq!(parsed.markdown_by_page.get("p1").unwrap(), "# Hello\n");
    }

    /// A database row usually has no body at all — 71% of pages in a
    /// measured workspace. That must round-trip as a page with no
    /// markdown, not as a missing page.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_page_with_no_body_still_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "row1".into(),
            parent_type: Some("data_source_id".into()),
            payload: Some(serde_json::to_string(&json!({"id": "row1"})).unwrap()),
            ..Default::default()
        }])
        .await
        .unwrap();
        // Sealed before render reads it, exactly as the download step does:
        // render pins HEAD, so an uncommitted row is invisible to it. Without
        // this the test asserts against the working set, which is the bug the
        // pinning work exists to remove.
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test fixture")
            .await
            .unwrap();
        // Closed, not dropped: `parse_api_dir` reopens this store.
        db.close().await;
        let parsed = parse_api_dir(&db_file, None).unwrap();
        assert_eq!(parsed.pages.len(), 1);
        assert!(parsed.markdown_by_page.is_empty());
    }
}
