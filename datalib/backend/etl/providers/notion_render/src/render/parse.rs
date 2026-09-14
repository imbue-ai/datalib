//! Read raw Notion payloads from the doltlite database written by
//! [`datalib_etl_notion::ingest`], narrowed to the buckets this run
//! renders: a page is one bucket, a comment thread another.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_notion::ingest::db::{db_path_for, AttachmentRow, RawDb};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;

/// Every table a page or a thread reads; the forward scan diffs each.
const TABLES: [&str; 6] = [
    "pages",
    "page_markdown",
    "comments",
    "comment_anchors",
    "notion_attachments",
    "users",
];

#[derive(Clone, Default)]
pub struct ParsedNotion {
    /// The pages to render, and the page of every thread to render — a
    /// thread reads its page's title. `render` tells the two apart.
    pub pages: Vec<Value>,
    /// Page bodies as Notion rendered them, keyed by page id. Attachment
    /// URLs are already slots, not signed links.
    pub markdown_by_page: HashMap<String, String>,
    /// The comments of every thread to render, `page_id` injected.
    pub comments: Vec<Value>,
    /// `user_id -> display name`. Notion resolves comment authors on
    /// the comment itself, so this is for the places it does not: a
    /// page's `created_by` / `last_edited_by`, and people properties.
    pub user_names: HashMap<String, String>,
    /// `block_id -> the text a comment on that block is anchored to`.
    /// A comment names its block and carries no quote, so without this
    /// a thread's anchor is an opaque uuid.
    pub anchor_text: HashMap<String, String>,
    /// `notion_attachments` row ids per page, bytes fetched or not.
    pub attachment_ids_by_page: HashMap<String, Vec<String>>,
    /// Attachment bytes per page, pre-loaded from the sibling CAS.
    /// Render calls `bundle.materialize_to_dir(<page_dir>/blobs)` once
    /// per page and resolves each slot with `bundle.filename_for`.
    pub blobs_by_page: HashMap<String, BlobBundle>,
    /// The commit everything was read at.
    pub head: Option<String>,
    /// The buckets to render — page ids and discussion ids; `None`
    /// renders everything.
    pub render: Option<HashSet<String>>,
}

impl ParsedNotion {
    pub fn renders(&self, bucket: &str) -> bool {
        self.render.as_ref().is_none_or(|r| r.contains(bucket))
    }
}

pub fn parse_api_dir(path: &Path, range: RawRange<'_>) -> Result<ParsedNotion> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        return Ok(ParsedNotion::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader_at(&db_path, range.pin).await? else {
                return Ok(ParsedNotion::default());
            };
            let parsed = load(&db, range).await;
            db.close().await;
            parsed
        })
    })
}

async fn load(db: &RawDb, range: RawRange<'_>) -> Result<ParsedNotion> {
    let pin = db.pin().expect("open_reader_at returns a pinned handle");
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;

    let user_names = db.load_user_names().await?;
    let anchor_text = db.load_comment_anchors().await?;
    let mut pages = db.load_pages().await?;
    let mut page_markdown = db.load_page_markdown().await?;
    let comments = db.load_comments().await?;
    let attachments = db.load_attachments().await?;

    let forward = changed.map(|changed| forward_buckets(&changed, &pages, &comments, &attachments));
    let render = range.narrow(forward.as_ref());
    let renders = |bucket: &str| render.as_ref().is_none_or(|r| r.contains(bucket));

    let comments: Vec<Value> = comments
        .into_iter()
        .filter(|(c, _)| discussion_of(c).is_some_and(renders))
        .map(|(mut c, page_id)| {
            if let (Some(obj), Some(pid)) = (c.as_object_mut(), page_id) {
                obj.entry("page_id").or_insert(Value::String(pid));
            }
            c
        })
        .collect();
    let thread_pages: HashSet<&str> = comments
        .iter()
        .filter_map(|c| c.get("page_id").and_then(Value::as_str))
        .collect();
    pages.retain(|p| id_of(p).is_some_and(|id| renders(id) || thread_pages.contains(id)));
    page_markdown.retain(|(id, _)| renders(id));

    let mut attachment_ids_by_page: HashMap<String, Vec<String>> = HashMap::new();
    let mut refs_by_page: HashMap<String, Vec<String>> = HashMap::new();
    for a in attachments {
        if !renders(&a.page_id) {
            continue;
        }
        attachment_ids_by_page
            .entry(a.page_id.clone())
            .or_default()
            .push(a.id);
        if a.blake3.is_some() {
            refs_by_page.entry(a.page_id).or_default().push(a.ref_id);
        }
    }
    let mut blobs_by_page: HashMap<String, BlobBundle> = HashMap::new();
    for (page_id, refs) in refs_by_page {
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let bundle = BlobBundle::load(
            db.pool(),
            db.cas().pool(),
            ATTACHMENTS_PROJECTION_SQL,
            &refs,
        )
        .await
        .with_context(|| format!("load attachments of page {page_id}"))?;
        if !bundle.is_empty() {
            blobs_by_page.insert(page_id, bundle);
        }
    }

    Ok(ParsedNotion {
        pages,
        markdown_by_page: page_markdown.into_iter().collect(),
        comments,
        user_names,
        anchor_text,
        attachment_ids_by_page,
        blobs_by_page,
        head: Some(pin.commit().to_string()),
        render,
    })
}

/// The buckets the changed rows name, through the rows still there: a
/// row that went names nothing here, and the bucket that read it is
/// the driver's to find. A page names its threads too, which read its
/// title.
fn forward_buckets(
    changed: &HashMap<String, HashSet<String>>,
    pages: &[Value],
    comments: &[(Value, Option<String>)],
    attachments: &[AttachmentRow],
) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    let of = |table: &str| changed.get(table);
    for table in ["pages", "page_markdown"] {
        if let Some(ids) = of(table) {
            out.extend(ids.iter().cloned());
        }
    }
    for (c, page_id) in comments {
        let Some(discussion) = discussion_of(c) else {
            continue;
        };
        let on_changed_page = of("pages")
            .zip(page_id.as_deref())
            .is_some_and(|(ids, pid)| ids.contains(pid));
        let changed_itself = of("comments")
            .zip(id_of(c))
            .is_some_and(|(ids, id)| ids.contains(id));
        let anchor_moved = of("comment_anchors")
            .zip(parent_block_of(c))
            .is_some_and(|(ids, block)| ids.contains(block));
        if on_changed_page || changed_itself || anchor_moved {
            out.insert(discussion.to_string());
        }
    }
    if let Some(ids) = of("notion_attachments") {
        for a in attachments {
            if ids.contains(&a.id) {
                out.insert(a.page_id.clone());
            }
        }
    }
    if let Some(users) = of("users") {
        for p in pages {
            let authored = ["created_by", "last_edited_by"]
                .iter()
                .filter_map(|k| p.get(k).and_then(|v| v.get("id")).and_then(Value::as_str))
                .any(|uid| users.contains(uid));
            if let (true, Some(id)) = (authored, id_of(p)) {
                out.insert(id.to_string());
            }
        }
    }
    out
}

fn id_of(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

fn discussion_of(c: &Value) -> Option<&str> {
    c.get("discussion_id")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
}

/// The block a comment hangs off, when it hangs off one.
pub fn parent_block_of(c: &Value) -> Option<&str> {
    c.get("parent")
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("block_id"))
        .and_then(|p| p.get("block_id"))
        .and_then(Value::as_str)
}

/// Maps an image block's `ref_id` (`"{block_uuid}:image"`) to its CAS
/// `blake3`, through the pinned view: an edge read from the working set
/// can name a row the producer has not committed.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT ref_id, blake3,
           NULL AS content_type,
           NULL AS upstream_name
      FROM pinned_notion_attachments notion_attachments
     WHERE ref_id IN ({placeholders}) AND blake3 IS NOT NULL";

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_notion::ingest::db::{PageMarkdownUpsert, PageUpsert};
    use datalib_etl_notion::ingest::RawDb;
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

        let parsed = parse_api_dir(&db_file, RawRange::cold()).unwrap();
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
        let parsed = parse_api_dir(&db_file, RawRange::cold()).unwrap();
        assert_eq!(parsed.pages.len(), 1);
        assert!(parsed.markdown_by_page.is_empty());
    }
}
