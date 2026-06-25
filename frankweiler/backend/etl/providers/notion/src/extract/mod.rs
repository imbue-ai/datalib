//! Notion downloader: BFS-mirror pages via the official API, with
//! optional inbox discovery via the unofficial `getNotificationLog`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/<name>/raw/entities.doltlite_db`) — one row per page / block /
//! comment, full payload in a JSON column. See `DOLTLITE_RAW.md` for the
//! schema and rationale. The downstream `render_and_index_md::parse` and
//! `synthesize` stages consume the DB directly.

pub mod db;
pub mod official;
pub mod schema_raw;
pub mod unofficial;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::extract_run::ExtractRun;
use frankweiler_etl::http::{latchkey_curl, HttpRequest};
use serde::Serialize;
use serde_json::{json, Value};

pub use db::{db_path_for, BlockUpsert, LoadedRaw, PageState, RawDb};
pub use official::{NotionOfficialClient, NotionOfficialError};
pub use unofficial::{NotionUnofficialClient, NotionUnofficialError};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Path to the doltlite database file. The entity db lives inside
    /// the per-source directory as `entities.doltlite_db` (the dir is
    /// created if needed). Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. The sync orchestrator pre-
    /// opens at startup so a download isn't started against a DB we
    /// can't write to (and so the post-extract commit can run on the
    /// same connection — no reopen race).
    pub db: Option<RawDb>,
    /// Page IDs (dashed or undashed) to seed the BFS queue.
    pub subtree_pages: Vec<String>,
    /// If set, also discover pages via the unofficial getNotificationLog.
    pub inbox: bool,
    /// When false, walk the inbox to discover referenced page IDs but
    /// don't actually BFS into them. Defaults to true.
    pub inbox_mirror_referenced: bool,
    /// Restrict inbox discovery to one space id.
    pub space: Option<String>,
    pub notification_page_size: u32,
    pub max_notification_pages: u32,
    pub inbox_types: Vec<String>,
    pub max_pages: usize,
    /// Single-page mode — short-circuit. Fetch only this page.
    pub page: Option<String>,
    /// When true, ignore subtree / inbox / page and re-fetch every row
    /// the DB currently has marked as failed or empty-with-attempts.
    pub retry_failed: bool,
    pub sleep_between: Duration,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            db_path: PathBuf::new(),
            db: None,
            subtree_pages: Vec::new(),
            inbox: false,
            inbox_mirror_referenced: true,
            space: None,
            notification_page_size: 40,
            max_notification_pages: 50,
            inbox_types: vec!["unread_and_read".into()],
            max_pages: 5000,
            page: None,
            retry_failed: false,
            sleep_between: Duration::ZERO,
            progress: frankweiler_etl::progress::Progress::noop(),
            control: frankweiler_etl::control::ExtractControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub new_pages: usize,
    pub upd_pages: usize,
    pub new_blocks: usize,
    pub upd_blocks: usize,
    pub new_comments: usize,
    pub upd_comments: usize,
    pub skipped_pages: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub official_requests: u64,
    pub unofficial_requests: u64,
}

/// Extract the URL of an image block's underlying file. Notion stores
/// uploads under `image.file.url` (signed S3, rotates) and externally
/// hosted images under `image.external.url`.
fn image_url_and_kind(block: &Value) -> Option<(String, &'static str)> {
    let img = block.get("image")?;
    if let Some(u) = img
        .get("external")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
    {
        return Some((u.to_string(), "external"));
    }
    if let Some(u) = img
        .get("file")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
    {
        return Some((u.to_string(), "notion_hosted"));
    }
    None
}

/// True when `url`'s host is a Notion-owned domain (and so its fetch should
/// go through latchkey). Everything else — chiefly the pre-signed S3 links
/// Notion hands out for uploaded files — is fetched with plain curl. Crude
/// host extraction (no `url` crate dep): take the chars between `://` and the
/// next `/`, `?`, or `#`, drop any `user@` and `:port`.
fn host_is_notion(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    host == "notion.so"
        || host.ends_with(".notion.so")
        || host == "notion.com"
        || host.ends_with(".notion.com")
}

/// Fetch every image block's bytes that we don't already have on file.
/// Errors are recorded against the blob row and don't fail the sync —
/// the page mirror has already landed and a later `--retry-failed` can
/// pick the broken blob up.
async fn fetch_image_blobs(db: &RawDb, blocks: &[Value], summary: &mut FetchSummary) -> Result<()> {
    for block in blocks {
        if block.get("type").and_then(|v| v.as_str()) != Some("image") {
            continue;
        }
        let Some(block_id) = block.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((url, kind)) = image_url_and_kind(block) else {
            continue;
        };
        let blob_id = format!("{block_id}:image");
        if db.blob_exists(&blob_id).await.unwrap_or(false) {
            summary.skipped_blobs += 1;
            continue;
        }
        // `kind` (notion_hosted / notion_external) used to live on
        // `blob_refs.kind`. We've retired blob_refs; the edge table
        // only stores `(block_id, ref_id, blake3)`, so the kind tag
        // is dropped. If a future query wants it, the block's payload
        // still carries the `image.type` upstream.
        let _ = kind;
        // Notion's file URLs are pre-signed S3 links (e.g.
        // `prod-files-secure.s3.<region>.amazonaws.com/…?X-Amz-Signature=…`)
        // that carry their own auth in the query string — they need no
        // latchkey credential. Routing them through the shim makes it try to
        // resolve an `aws` credential it doesn't have and fail the fetch. Only
        // URLs actually on a Notion host go through latchkey; send everything
        // else via plain curl.
        let mut req = HttpRequest::get("notion", &url);
        if !host_is_notion(&url) {
            req = req.plain();
        }
        match latchkey_curl(&req).await {
            Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                let content_type = resp.header("content-type");
                if let Err(e) = db
                    .store_blob(block_id, &blob_id, content_type, &resp.body)
                    .await
                {
                    tracing::warn!(blob = %blob_id, error = %e, "blob upsert failed");
                    summary.failed_blobs += 1;
                } else {
                    summary.new_blobs += 1;
                }
            }
            Ok(resp) => {
                let msg = format!("HTTP {}", resp.status);
                tracing::warn!(blob = %blob_id, url = %url, error = %msg, "blob fetch non-2xx");
                let _ = db.record_blob_error(block_id, &blob_id).await;
                summary.failed_blobs += 1;
            }
            Err(e) => {
                let msg = format!("{e}");
                tracing::warn!(blob = %blob_id, url = %url, error = %msg, "blob fetch failed");
                let _ = db.record_blob_error(block_id, &blob_id).await;
                summary.failed_blobs += 1;
            }
        }
    }
    Ok(())
}

fn format_uuid(s: &str) -> String {
    let raw: String = s.chars().filter(|c| *c != '-').collect();
    if raw.len() != 32 {
        return s.into();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    )
}

fn parent_of(page: &Value) -> Option<String> {
    let p = page.get("parent")?;
    p.get("page_id")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("block_id").and_then(|v| v.as_str()))
        .or_else(|| p.get("database_id").and_then(|v| v.as_str()))
        .or_else(|| p.get("workspace").map(|_| "workspace"))
        .map(String::from)
}

fn block_parent(block: &Value) -> Option<String> {
    let p = block.get("parent")?;
    p.get("block_id")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("page_id").and_then(|v| v.as_str()))
        .map(String::from)
}

fn comment_parent(c: &Value) -> Option<String> {
    let p = c.get("parent")?;
    p.get("block_id")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("page_id").and_then(|v| v.as_str()))
        .map(String::from)
}

#[tracing::instrument(skip(client), fields(parent_id, pages, children))]
async fn fetch_all_children(client: &NotionOfficialClient, parent_id: &str) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        let resp = client
            .get_block_children(parent_id, cursor.as_deref())
            .await?;
        pages += 1;
        if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
            out.extend(arr.iter().cloned());
        }
        if !resp
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            tracing::Span::current().record("pages", pages);
            tracing::Span::current().record("children", out.len());
            return Ok(out);
        }
        let nc = resp.get("next_cursor").and_then(|v| v.as_str());
        if let Some(c) = nc {
            cursor = Some(c.to_string());
        } else {
            tracing::Span::current().record("pages", pages);
            tracing::Span::current().record("children", out.len());
            return Ok(out);
        }
    }
}

#[tracing::instrument(skip(client), fields(page_id, blocks, recursed))]
async fn walk_page_blocks(client: &NotionOfficialClient, page_id: &str) -> Result<Vec<Value>> {
    let mut collected: Vec<Value> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    queue.push_back(page_id.to_string());
    let mut recursed: u32 = 0;
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid.clone()) {
            continue;
        }
        let children = fetch_all_children(client, &pid).await?;
        for ch in children {
            let t = ch.get("type").and_then(|v| v.as_str()).unwrap_or("");
            collected.push(ch.clone());
            if t == "child_page" || t == "child_database" {
                continue;
            }
            if ch
                .get("has_children")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(id) = ch.get("id").and_then(|v| v.as_str()) {
                    recursed += 1;
                    queue.push_back(id.into());
                }
            }
        }
    }
    tracing::Span::current().record("blocks", collected.len());
    tracing::Span::current().record("recursed", recursed);
    Ok(collected)
}

fn child_page_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("child_page"))
        .filter_map(|b| b.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

#[tracing::instrument(skip(client), fields(page_id, pages, comments))]
async fn fetch_all_comments(client: &NotionOfficialClient, page_id: &str) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        let resp = client.get_comments(page_id, cursor.as_deref()).await?;
        pages += 1;
        if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
            out.extend(arr.iter().cloned());
        }
        if !resp
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            tracing::Span::current().record("pages", pages);
            tracing::Span::current().record("comments", out.len());
            return Ok(out);
        }
        let nc = resp.get("next_cursor").and_then(|v| v.as_str());
        if let Some(c) = nc {
            cursor = Some(c.to_string());
        } else {
            tracing::Span::current().record("pages", pages);
            tracing::Span::current().record("comments", out.len());
            return Ok(out);
        }
    }
}

fn extract_inbox_pages(rm: &Value) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let Some(activity) = rm.get("activity").and_then(|v| v.as_object()) else {
        return seen;
    };
    for payload in activity.values() {
        let value = payload
            .get("value")
            .and_then(|v| v.get("value").or(Some(v)))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(nav) = value.get("navigable_block_id").and_then(|v| v.as_str()) {
            if !seen.iter().any(|x| x == nav) {
                seen.push(nav.into());
            }
        }
    }
    seen
}

async fn walk_inbox(
    uo: &NotionUnofficialClient,
    space_id: &str,
    page_size: u32,
    max_pages: u32,
    types: &[String],
) -> Result<Vec<String>> {
    let mut seen: Vec<String> = Vec::new();
    for t in types {
        let mut cursor: Option<Value> = None;
        for _ in 0..max_pages {
            let resp = uo
                .get_notification_log(space_id, page_size, cursor.as_ref(), t)
                .await?;
            let rm = resp
                .get("recordMap")
                .cloned()
                .unwrap_or(Value::Object(Default::default()));
            for r in extract_inbox_pages(&rm) {
                if !seen.contains(&r) {
                    seen.push(r);
                }
            }
            let ids_empty = resp
                .get("notificationIds")
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true);
            let next = resp.get("cursor").cloned();
            if next.is_none() || ids_empty {
                break;
            }
            cursor = next;
        }
    }
    Ok(seen)
}

/// Per-page work. Returns the page's blocks so the BFS driver can find
/// `child_page` descendants. Records errors against the DB rather than
/// short-circuiting the whole sync.
#[tracing::instrument(skip_all, fields(page_id = %pid, origin = %origin, blocks, comments, page_ms, blocks_ms, comments_ms, skipped))]
#[allow(clippy::too_many_arguments)]
async fn mirror_page(
    client: &NotionOfficialClient,
    db: &RawDb,
    pid: &str,
    origin: &'static str,
    page_states: &mut HashMap<String, PageState>,
    summary: &mut FetchSummary,
    seen_last_edited_from_list: Option<&str>,
) -> Result<Vec<Value>> {
    // Skip detail fetch when we already have this page's full payload
    // and the upstream `last_edited_time` hasn't moved. Discovery
    // (list/search) gives us the (id, last_edited_time) pair; we trust it.
    if let (Some(state), Some(incoming)) = (page_states.get(pid), seen_last_edited_from_list) {
        if state.has_payload && state.last_edited_time.as_deref() == Some(incoming) {
            tracing::Span::current().record("skipped", true);
            summary.skipped_pages += 1;
            // We still need the page's blocks to discover child_pages
            // for BFS; but if our local copy is current we already have
            // them — caller will rely on its own existing-blocks view.
            return Ok(Vec::new());
        }
    }

    let page_t = std::time::Instant::now();
    let page = match client.get_page(pid).await {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e}");
            tracing::warn!(page = pid, error = %msg, "page fetch failed; recording");
            let _ = db.record_page_error(pid, &msg).await;
            return Ok(Vec::new());
        }
    };
    let page_ms = page_t.elapsed().as_millis() as u64;
    let was_present = page_states.get(pid).map(|s| s.has_payload).unwrap_or(false);
    let prior_last_edited = page_states
        .get(pid)
        .and_then(|s| s.last_edited_time.clone());
    let last_edited = page
        .get("last_edited_time")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Post-detail incrementality skip. BFS-discovered pages don't
    // come with an `incoming` last_edited_time hint (no list endpoint),
    // so the pre-fetch skip at the top of this function can't fire
    // for them. But once the page detail is in hand, we know the
    // upstream `last_edited_time` — and if it matches what we already
    // stored, the children/comments/etc. can't have changed either.
    // Returning a synthetic block array of stored `child_page` ids
    // keeps BFS recursing into known children (in case a child's own
    // `last_edited_time` advanced even when the parent's didn't).
    if was_present && last_edited.is_some() && last_edited == prior_last_edited {
        summary.skipped_pages += 1;
        let child_ids = db
            .stored_child_page_ids(pid)
            .await
            .with_context(|| format!("stored child_page ids for {pid}"))?;
        tracing::Span::current().record("skipped", true);
        tracing::debug!(
            page = pid,
            child_pages = child_ids.len(),
            page_fetch_ms = page_ms,
            "page unchanged: skipped block walk; recursing into known children"
        );
        return Ok(child_ids
            .into_iter()
            .map(|id| serde_json::json!({"type": "child_page", "id": id}))
            .collect());
    }

    let parent_id = parent_of(&page);
    let payload = serde_json::to_string(&page).ok();
    db.upsert_pages(&[(pid.to_string(), parent_id, last_edited.clone(), payload)])
        .await
        .with_context(|| format!("upsert page {pid}"))?;
    page_states.insert(
        pid.into(),
        PageState {
            last_edited_time: last_edited,
            has_payload: true,
        },
    );
    if was_present {
        summary.upd_pages += 1;
    } else {
        summary.new_pages += 1;
    }

    let blocks_t = std::time::Instant::now();
    let blocks = match walk_page_blocks(client, pid).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(page = pid, error = %e, "blocks fetch failed; skipping");
            return Ok(Vec::new());
        }
    };
    let blocks_ms = blocks_t.elapsed().as_millis() as u64;
    let mut block_rows: Vec<db::BlockUpsert> = Vec::with_capacity(blocks.len());
    // `idx` is the block's index within this page's BFS walk — its
    // stable position for layout. Persisted as `blocks.page_order` so
    // render reproduces the page top-to-bottom regardless of the dolt
    // primary-key ordering of UUIDs.
    for (idx, b) in blocks.iter().enumerate() {
        let Some(id) = b.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let parent = block_parent(b);
        let last = b
            .get("last_edited_time")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload = serde_json::to_string(b).ok();
        block_rows.push(db::BlockUpsert {
            id: id.into(),
            parent_id: parent,
            page_id: Some(pid.into()),
            page_order: Some(idx as i64),
            last_edited_time: last,
            payload,
        });
    }
    summary.upd_blocks += block_rows.len(); // we don't distinguish new vs upd for blocks anymore
    db.upsert_blocks(&block_rows)
        .await
        .with_context(|| format!("upsert blocks for {pid}"))?;

    // Fetch image blobs inline — the per-block GET is small and lets a
    // single sync run produce a self-contained DB. Per the design doc
    // we skip refetch when we already have bytes (signed URLs rotate,
    // bytes don't).
    if let Err(e) = fetch_image_blobs(db, &blocks, summary).await {
        tracing::warn!(page = pid, error = %e, "blob pass failed; continuing");
    }

    let comments_t = std::time::Instant::now();
    let comments = fetch_all_comments(client, pid).await.unwrap_or_default();
    let comments_ms = comments_t.elapsed().as_millis() as u64;
    if !comments.is_empty() {
        let mut comment_rows: Vec<(String, String, Option<String>, String)> =
            Vec::with_capacity(comments.len());
        for c in &comments {
            let Some(id) = c.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            // Comments must hang off something — fall back to the page
            // we were fetching for when parent is missing.
            let parent = comment_parent(c).unwrap_or_else(|| pid.to_string());
            let payload = serde_json::to_string(c).unwrap_or_else(|_| "null".into());
            comment_rows.push((id.into(), parent, Some(pid.into()), payload));
        }
        summary.upd_comments += comment_rows.len();
        db.upsert_comments(&comment_rows)
            .await
            .with_context(|| format!("upsert comments for {pid}"))?;
    }

    let span = tracing::Span::current();
    span.record("blocks", blocks.len());
    span.record("comments", comments.len());
    span.record("page_ms", page_ms);
    span.record("blocks_ms", blocks_ms);
    span.record("comments_ms", comments_ms);
    tracing::info!(
        page_id = %pid,
        origin = %origin,
        blocks = blocks.len(),
        comments = comments.len(),
        page_ms,
        blocks_ms,
        comments_ms,
        "mirror_page done"
    );
    Ok(blocks)
}

#[allow(clippy::too_many_arguments)]
async fn bfs_drain(
    client: &NotionOfficialClient,
    db: &RawDb,
    opts: &FetchOptions,
    origin: &'static str,
    mut queue: VecDeque<String>,
    queued: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    page_states: &mut HashMap<String, PageState>,
    summary: &mut FetchSummary,
    single_page: bool,
) -> Result<()> {
    while let Some(pid) = queue.pop_front() {
        if visited.len() >= opts.max_pages {
            break;
        }
        if !visited.insert(pid.clone()) {
            continue;
        }
        opts.progress
            .set_length(Some((visited.len() + queue.len()) as u64));
        opts.progress.inc(1);
        opts.progress.set_message(&pid);
        // No incoming `last_edited_time` from a list pass here yet —
        // Notion's official API has no global list endpoint, so we
        // can't know the upstream value without fetching the page.
        // Skip-on-unchanged for those will land when we add cursored
        // search; for now every queued page is fetched.
        let blocks = mirror_page(client, db, &pid, origin, page_states, summary, None).await?;
        if !single_page {
            for cid in child_page_ids(&blocks) {
                if queued.insert(cid.clone()) {
                    queue.push_back(cid);
                }
            }
        }
        if opts.sleep_between > Duration::ZERO {
            tokio::time::sleep(opts.sleep_between).await;
        }
    }
    Ok(())
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();

    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };
    if opts.control.reset_and_redownload {
        tracing::info!(event = "notion_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        tracing::info!(event = "notion_refetch_blobs");
        frankweiler_etl::doltlite_raw::truncate_data_tables(
            db.pool(),
            &["notion_image_attachments"],
        )
        .await
        .context("truncate notion_image_attachments before refetch")?;
    }
    let run_config = json!({
        "subtree_pages": opts.subtree_pages,
        "inbox": opts.inbox,
        "inbox_mirror_referenced": opts.inbox_mirror_referenced,
        "space": opts.space,
        "notification_page_size": opts.notification_page_size,
        "max_notification_pages": opts.max_notification_pages,
        "inbox_types": opts.inbox_types,
        "max_pages": opts.max_pages,
        "page": opts.page,
        "retry_failed": opts.retry_failed,
    });
    let run = ExtractRun::start(db.pool(), &run_config).await?;

    let official = NotionOfficialClient::new();
    let mut summary = FetchSummary::default();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut page_states = db.page_states().await?;

    // Run the actual work. We capture the result so we can always stamp
    // the sync_runs row with finish status — even on error.
    let work = async {
        if opts.retry_failed {
            let span = tracing::info_span!("notion_retry_pass");
            let _enter = span.enter();
            let failed = db.failed_page_ids().await?;
            tracing::info!(count = failed.len(), "retrying failed pages");
            let mut q: VecDeque<String> = VecDeque::new();
            for id in failed {
                if queued.insert(id.clone()) {
                    q.push_back(id);
                }
            }
            bfs_drain(
                &official,
                &db,
                &opts,
                "retry",
                q,
                &mut queued,
                &mut visited,
                &mut page_states,
                &mut summary,
                true,
            )
            .await?;
            return Ok::<(), anyhow::Error>(());
        }

        if let Some(single) = opts.page.as_deref() {
            let id = format_uuid(single);
            queued.insert(id.clone());
            let mut q = VecDeque::new();
            q.push_back(id);
            bfs_drain(
                &official,
                &db,
                &opts,
                "single",
                q,
                &mut queued,
                &mut visited,
                &mut page_states,
                &mut summary,
                true,
            )
            .await?;
            return Ok(());
        }

        // Pass 1: subtree seeds.
        {
            let span = tracing::info_span!("notion_subtree_pass", pages = opts.subtree_pages.len());
            let _enter = span.enter();
            let mut subtree_queue: VecDeque<String> = VecDeque::new();
            for raw in &opts.subtree_pages {
                let stripped = frankweiler_etl::ids::normalize_id_token(raw);
                let id = format_uuid(&stripped);
                if queued.insert(id.clone()) {
                    subtree_queue.push_back(id);
                }
            }
            bfs_drain(
                &official,
                &db,
                &opts,
                "subtree",
                subtree_queue,
                &mut queued,
                &mut visited,
                &mut page_states,
                &mut summary,
                false,
            )
            .await?;
            tracing::info!(visited = visited.len(), "subtree pass done");
        }

        // Pass 2: inbox.
        if opts.inbox {
            let span = tracing::info_span!("notion_inbox_pass");
            let _enter = span.enter();
            let uo = NotionUnofficialClient::new();
            uo.load_user_content().await?;
            let spaces_resp = uo.get_spaces().await?;
            let space_ids: Vec<String> = if let Some(s) = opts.space.as_deref() {
                vec![s.into()]
            } else {
                let mut out: Vec<String> = Vec::new();
                if let Some(obj) = spaces_resp.as_object() {
                    for v in obj.values() {
                        let Some(space) = v.get("space").and_then(|v| v.as_object()) else {
                            continue;
                        };
                        for sid in space.keys() {
                            if !out.contains(sid) {
                                out.push(sid.clone());
                            }
                        }
                    }
                }
                out
            };
            tracing::info!(?space_ids, "inbox spaces");
            let mut inbox_queue: VecDeque<String> = VecDeque::new();
            let mut total_refs = 0usize;
            let mut already_mirrored = 0usize;
            for sid in space_ids {
                let refs = walk_inbox(
                    &uo,
                    &sid,
                    opts.notification_page_size,
                    opts.max_notification_pages,
                    &opts.inbox_types,
                )
                .await?;
                tracing::info!(space = %sid, found = refs.len(), "inbox refs");
                total_refs += refs.len();
                for rid in refs {
                    let pid = format_uuid(&rid);
                    if visited.contains(&pid) {
                        already_mirrored += 1;
                        continue;
                    }
                    if queued.insert(pid.clone()) {
                        inbox_queue.push_back(pid);
                    }
                }
            }
            summary.unofficial_requests = uo.request_count();
            if !opts.inbox_mirror_referenced {
                tracing::info!(
                    refs = total_refs,
                    already_mirrored,
                    "inbox refs collected; not mirroring (inbox_mirror_referenced=false)"
                );
            } else {
                tracing::info!(
                    queued = inbox_queue.len(),
                    already_mirrored,
                    "inbox pages queued for mirror"
                );
                bfs_drain(
                    &official,
                    &db,
                    &opts,
                    "inbox",
                    inbox_queue,
                    &mut queued,
                    &mut visited,
                    &mut page_states,
                    &mut summary,
                    false,
                )
                .await?;
            }
        }
        Ok(())
    };

    let result = work.await;
    summary.official_requests = official.request_count();
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// Public re-export: the legacy entity name constants are still
/// referenced in a few places. They no longer correspond to on-disk
/// paths but stay around as logical identifiers.
pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_BLOCK: &str = "notion_official_block";
pub const ENTITY_COMMENT: &str = "notion_official_comment";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uuid_handles_dashed_and_undashed() {
        assert_eq!(
            format_uuid("f9a3f309bde54852944042374cc01dc5"),
            "f9a3f309-bde5-4852-9440-42374cc01dc5"
        );
        let already = "f9a3f309-bde5-4852-9440-42374cc01dc5";
        assert_eq!(format_uuid(already), already);
    }
}
