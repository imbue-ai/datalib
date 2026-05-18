//! Notion downloader: BFS-mirror pages via the official API, with
//! optional inbox discovery via the unofficial `getNotificationLog`.
//!
//! Port of `src/download/notion_official.py`. Writes to an event-store
//! JSONL layout under `<out_dir>/notion_official_<entity>/{created,updated}/events.jsonl`,
//! consumed by [`crate::translate::parse`].

pub mod official;
pub mod unofficial;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use serde_json::{json, Map, Value};

pub use official::{NotionOfficialClient, NotionOfficialError};
pub use unofficial::{NotionUnofficialClient, NotionUnofficialError};

pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_BLOCK: &str = "notion_official_block";
pub const ENTITY_COMMENT: &str = "notion_official_comment";

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub out_dir: PathBuf,
    /// Page IDs (dashed or undashed) to seed the BFS queue.
    pub subtree_pages: Vec<String>,
    /// If set, also discover pages via the unofficial getNotificationLog.
    pub inbox: bool,
    /// Restrict inbox discovery to one space id.
    pub space: Option<String>,
    pub notification_page_size: u32,
    pub max_notification_pages: u32,
    pub inbox_types: Vec<String>,
    pub max_pages: usize,
    /// Single-page mode — short-circuit. Fetch only this page.
    pub page: Option<String>,
    pub sleep_between: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            out_dir: PathBuf::new(),
            subtree_pages: Vec::new(),
            inbox: false,
            space: None,
            notification_page_size: 40,
            max_notification_pages: 50,
            inbox_types: vec!["unread_and_read".into()],
            max_pages: 5000,
            page: None,
            sleep_between: Duration::ZERO,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FetchSummary {
    pub new_pages: usize,
    pub upd_pages: usize,
    pub new_blocks: usize,
    pub upd_blocks: usize,
    pub new_comments: usize,
    pub upd_comments: usize,
    pub official_requests: u64,
    pub unofficial_requests: u64,
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

fn key_id(rec: &Value) -> String {
    rec.get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn page_record(page: &Value) -> Value {
    let mut k = Map::new();
    if let Some(v) = page.get("id").cloned() {
        k.insert("id".into(), v);
    }
    k.insert(
        "last_edited_time".into(),
        page.get("last_edited_time").cloned().unwrap_or(Value::Null),
    );
    k.insert("parent".into(), page.get("parent").cloned().unwrap_or(Value::Null));
    make_record(k, page.clone())
}

fn block_record(block: &Value, page_id: &str) -> Value {
    let mut k = Map::new();
    if let Some(v) = block.get("id").cloned() {
        k.insert("id".into(), v);
    }
    k.insert("page_id".into(), Value::String(page_id.into()));
    k.insert(
        "type".into(),
        block.get("type").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "last_edited_time".into(),
        block.get("last_edited_time").cloned().unwrap_or(Value::Null),
    );
    make_record(k, block.clone())
}

fn comment_record(comment: &Value, page_id: &str) -> Value {
    let parent = comment.get("parent").cloned().unwrap_or(Value::Null);
    let mut k = Map::new();
    if let Some(v) = comment.get("id").cloned() {
        k.insert("id".into(), v);
    }
    k.insert("page_id".into(), Value::String(page_id.into()));
    k.insert(
        "discussion_id".into(),
        comment.get("discussion_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "parent_block_id".into(),
        parent.get("block_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "parent_page_id".into(),
        parent.get("page_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "created_time".into(),
        comment.get("created_time").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "last_edited_time".into(),
        comment.get("last_edited_time").cloned().unwrap_or(Value::Null),
    );
    make_record(k, comment.clone())
}

async fn fetch_all_children(
    client: &NotionOfficialClient,
    parent_id: &str,
) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let resp = client.get_block_children(parent_id, cursor.as_deref()).await?;
        if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
            out.extend(arr.iter().cloned());
        }
        if !resp.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(out);
        }
        let nc = resp.get("next_cursor").and_then(|v| v.as_str());
        if let Some(c) = nc {
            cursor = Some(c.to_string());
        } else {
            return Ok(out);
        }
    }
}

async fn walk_page_blocks(
    client: &NotionOfficialClient,
    page_id: &str,
) -> Result<Vec<Value>> {
    let mut collected: Vec<Value> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    queue.push_back(page_id.to_string());
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
            if ch.get("has_children").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(id) = ch.get("id").and_then(|v| v.as_str()) {
                    queue.push_back(id.into());
                }
            }
        }
    }
    Ok(collected)
}

fn child_page_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("child_page"))
        .filter_map(|b| b.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

async fn fetch_all_comments(
    client: &NotionOfficialClient,
    page_id: &str,
) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let resp = client.get_comments(page_id, cursor.as_deref()).await?;
        if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
            out.extend(arr.iter().cloned());
        }
        if !resp.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(out);
        }
        let nc = resp.get("next_cursor").and_then(|v| v.as_str());
        if let Some(c) = nc {
            cursor = Some(c.to_string());
        } else {
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
        // payload.value (or .value.value) carries activity record
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
                if !seen.iter().any(|x| *x == r) {
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

async fn mirror_page(
    client: &NotionOfficialClient,
    out_dir: &Path,
    pid: &str,
    existing_pages: &mut HashMap<String, Value>,
    existing_blocks: &mut HashMap<String, Value>,
    existing_comments: &mut HashMap<String, Value>,
    summary: &mut FetchSummary,
) -> Result<Vec<Value>> {
    let page = match client.get_page(pid).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(page = pid, error = %e, "page fetch failed; skipping");
            return Ok(Vec::new());
        }
    };
    let prec = page_record(&page);
    let counts =
        diff_and_save(out_dir, ENTITY_PAGE, &[prec.clone()], existing_pages, key_id)?;
    summary.new_pages += counts.new;
    summary.upd_pages += counts.updated;
    existing_pages.insert(pid.into(), prec);

    let blocks = match walk_page_blocks(client, pid).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(page = pid, error = %e, "blocks fetch failed; skipping");
            return Ok(Vec::new());
        }
    };
    let block_records: Vec<Value> = blocks.iter().map(|b| block_record(b, pid)).collect();
    let counts =
        diff_and_save(out_dir, ENTITY_BLOCK, &block_records, existing_blocks, key_id)?;
    summary.new_blocks += counts.new;
    summary.upd_blocks += counts.updated;
    for br in &block_records {
        if let Some(id) = br.get("id").and_then(|v| v.as_str()) {
            existing_blocks.insert(id.into(), br.clone());
        }
    }

    let comments = fetch_all_comments(client, pid).await.unwrap_or_default();
    if !comments.is_empty() {
        let crecs: Vec<Value> = comments.iter().map(|c| comment_record(c, pid)).collect();
        let counts =
            diff_and_save(out_dir, ENTITY_COMMENT, &crecs, existing_comments, key_id)?;
        summary.new_comments += counts.new;
        summary.upd_comments += counts.updated;
        for cr in &crecs {
            if let Some(id) = cr.get("id").and_then(|v| v.as_str()) {
                existing_comments.insert(id.into(), cr.clone());
            }
        }
    }
    Ok(blocks)
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create {}", opts.out_dir.display()))?;
    auto_set_latchkey_curl()?;

    let official = NotionOfficialClient::new();

    let mut existing_pages = load_latest_by_key(&opts.out_dir, ENTITY_PAGE, key_id)?;
    let mut existing_blocks = load_latest_by_key(&opts.out_dir, ENTITY_BLOCK, key_id)?;
    let mut existing_comments = load_latest_by_key(&opts.out_dir, ENTITY_COMMENT, key_id)?;

    let mut summary = FetchSummary::default();

    // Build BFS queue.
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut queued: HashSet<String> = HashSet::new();

    if let Some(single) = opts.page.as_deref() {
        let id = format_uuid(single);
        queue.push_back(id.clone());
        queued.insert(id);
    } else {
        for raw in &opts.subtree_pages {
            let id = format_uuid(raw);
            if queued.insert(id.clone()) {
                queue.push_back(id);
            }
        }
        if opts.inbox {
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
                for rid in refs {
                    let pid = format_uuid(&rid);
                    if queued.insert(pid.clone()) {
                        queue.push_back(pid);
                    }
                }
            }
            summary.unofficial_requests = uo.request_count();
        }
    }

    let mut visited: HashSet<String> = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if visited.len() >= opts.max_pages {
            break;
        }
        if !visited.insert(pid.clone()) {
            continue;
        }
        let blocks = mirror_page(
            &official,
            &opts.out_dir,
            &pid,
            &mut existing_pages,
            &mut existing_blocks,
            &mut existing_comments,
            &mut summary,
        )
        .await?;
        // Single-page mode means "just this page" — don't walk descendants.
        if opts.page.is_none() {
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
    summary.official_requests = official.request_count();
    let _ = json!(summary.official_requests); // silence unused-import warning
    Ok(summary)
}

/// If `LATCHKEY_CURL` is unset, point it at the in-tree
/// `latchkey-curl-shim` binary so Cloudflare-fronted unofficial endpoints
/// clear the green path automatically.
fn auto_set_latchkey_curl() -> Result<()> {
    if std::env::var_os("LATCHKEY_CURL").is_some() {
        return Ok(());
    }
    // Best-effort: look for a sibling binary in the standard cargo
    // target dirs under the current crate.
    let candidates = [
        "target/debug/latchkey-curl-shim",
        "target/release/latchkey-curl-shim",
        "frankweiler/backend/target/debug/latchkey-curl-shim",
        "frankweiler/backend/target/release/latchkey-curl-shim",
    ];
    for c in candidates {
        if std::path::Path::new(c).exists() {
            std::env::set_var("LATCHKEY_CURL", c);
            tracing::info!(shim = c, "auto-set LATCHKEY_CURL");
            return Ok(());
        }
    }
    Ok(())
}

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
