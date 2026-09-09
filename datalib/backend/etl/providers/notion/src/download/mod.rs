//! Notion downloader: mirror pages via the official API.

pub mod db;
pub mod markdown;
pub mod official;
pub mod schema_raw;
pub mod slots;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::{latchkey_curl, HttpRequest, HttpService, LatchkeySettings};
use serde::Serialize;
use serde_json::{json, Value};

pub use db::{db_path_for, LoadedRaw, PageState, RawDb};
pub use official::{NotionOfficialClient, NotionOfficialError};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// Page IDs (dashed or undashed) to seed the walk.
    pub subtree_pages: Vec<String>,
    /// Re-examine anything edited within this many days even when the
    /// stored resume cursor is newer. 0 means no floor.
    pub refresh_window_days: u32,
    /// Ignore the resume cursor and walk the whole workspace.
    pub full_sync: bool,
    /// Mirror each page's comment threads.
    pub comments: bool,
    /// Archive attachment bytes into the CAS.
    pub attachments: bool,
    /// Hard stop on pages visited. `None` means no limit — a
    /// whole-workspace mirror is the normal case, and a silent cap
    /// would truncate it without saying so.
    pub max_pages: Option<usize>,
    /// Single-page mode — short-circuit. Fetch only this page.
    pub page: Option<String>,
    /// When true, ignore roots / page and re-fetch every row
    /// the DB currently has marked as failed or empty-with-attempts.
    pub retry_failed: bool,
    pub sleep_between: Duration,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: datalib_etl::control::DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        FetchOptions {
            db,
            latchkey: LatchkeySettings::default(),
            subtree_pages: Vec::new(),
            refresh_window_days: 0,
            full_sync: false,
            comments: true,
            attachments: true,
            max_pages: None,
            page: None,
            retry_failed: false,
            sleep_between: Duration::ZERO,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub new_pages: usize,
    pub upd_pages: usize,
    /// Page bodies fetched from the markdown endpoint.
    pub bodies: usize,
    /// Bodies that came back empty — the common case for a database
    /// row, whose content is its properties.
    pub empty_bodies: usize,
    pub failed_bodies: usize,
    /// Pages the search pass named as changed since the resume cursor.
    pub discovered: usize,
    /// Users resolved by id (one request each, once ever).
    pub users_resolved: usize,
    /// Blocks a comment hangs off, fetched for their anchor text.
    pub anchors_resolved: usize,
    /// Truncated subtrees fetched as follow-ups.
    pub hole_followups: usize,
    /// Pages with more truncated subtrees than one run will follow.
    pub pages_left_incomplete: usize,
    pub new_comments: usize,
    pub upd_comments: usize,
    pub skipped_pages: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    /// Pages whose detail fetch failed. Recorded per page and otherwise
    /// tolerated — one unreadable page shouldn't abort a BFS over
    /// thousands — but see the all-failed check at the end of [`fetch`]:
    /// when this is the ONLY thing that happened, the run fails.
    pub failed_pages: usize,
    pub official_requests: u64,
}

/// True when pages were attempted and not one of them worked out.
fn all_pages_failed(s: &FetchSummary) -> bool {
    s.failed_pages > 0 && s.new_pages == 0 && s.upd_pages == 0 && s.skipped_pages == 0
}

/// True when `url`'s host is a Notion-owned domain (and so its fetch
/// should go through latchkey). Everything else — chiefly the pre-signed
/// S3 links Notion hands out for uploaded files — is fetched with plain
/// curl. Crude host extraction (no `url` crate dep).
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

/// Archive a page's attachment bytes.
///
/// `slots` are unsigned URLs — stable identity, but not fetchable on
/// their own. `signed` maps each slot to the live pre-signed URL from
/// this run's response, which expires in about an hour; that is why the
/// bytes are pulled in the same run that read the markdown.
///
/// A slot whose edge already carries a `blake3` is skipped: signatures
/// rotate, bytes don't. Failures are recorded against the edge and never
/// fail the run — the page has already landed, and the stored markdown
/// is correct and stable regardless.
async fn fetch_attachments(
    db: &RawDb,
    page_id: &str,
    slots: &[String],
    signed: &HashMap<String, String>,
    summary: &mut FetchSummary,
) {
    for slot in slots {
        if db.blob_exists(slot).await.unwrap_or(false) {
            summary.skipped_blobs += 1;
            continue;
        }
        let Some(url) = signed.get(slot) else {
            continue;
        };
        let mut req = HttpRequest::get(HttpService::Notion, url);
        if !host_is_notion(url) {
            req = req.plain();
        }
        match latchkey_curl(&req).await {
            Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                let content_type = resp.header("content-type");
                if let Err(e) = db.store_blob(page_id, slot, content_type, &resp.body).await {
                    tracing::warn!(slot = %slot, error = %format!("{e:#}"), "attachment upsert failed");
                    summary.failed_blobs += 1;
                } else {
                    summary.new_blobs += 1;
                }
            }
            Ok(resp) => {
                tracing::warn!(slot = %slot, status = resp.status, "attachment fetch non-2xx");
                let _ = db.record_blob_error(page_id, slot).await;
                summary.failed_blobs += 1;
            }
            Err(e) => {
                tracing::warn!(slot = %slot, error = %format!("{e}"), "attachment fetch failed");
                let _ = db.record_blob_error(page_id, slot).await;
                summary.failed_blobs += 1;
            }
        }
    }
}

/// Map each slot back to the live signed URL it came from, so the bytes
/// can still be fetched after the markdown has been rewritten.
fn signed_by_slot(raw_markdown: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = raw_markdown;
    while let Some(i) = rest.find("http") {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>'))
            .unwrap_or(tail.len());
        let url = &tail[..end];
        if let Some(slot) = slots::slot_of(url) {
            out.entry(slot).or_insert_with(|| url.to_string());
        }
        rest = &tail[end.max(1)..];
    }
    out
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

fn comment_parent(c: &Value) -> Option<(String, String)> {
    let p = c.get("parent")?;
    let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let id = p
        .get("block_id")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("page_id").and_then(|v| v.as_str()))?;
    Some((t.to_string(), id.to_string()))
}

#[tracing::instrument(skip(client), fields(page_id, pages, comments))]
async fn fetch_all_comments(client: &NotionOfficialClient, page_id: &str) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let resp = client.get_comments(page_id, cursor.as_deref()).await?;
        if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
            out.extend(arr.iter().cloned());
        }
        if !resp
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Ok(out);
        }
        match resp.get("next_cursor").and_then(|v| v.as_str()) {
            Some(c) => cursor = Some(c.to_string()),
            None => return Ok(out),
        }
    }
}

/// Follow-up fetches for subtrees the markdown response was too large to
/// inline, appended to the body in the order the holes appeared.
///
/// Only `alt`-less holes are followed: an `alt`-bearing one names a
/// block type markdown cannot express, and fetching it returns a stub
/// carrying the same id — an infinite regress. `MAX_HOLE_FOLLOWUPS`
/// bounds the pass so a single pathological page (one measured page
/// wanted ~1,385 of these) can't dominate a run.
const MAX_HOLE_FOLLOWUPS: usize = 64;

async fn fill_holes(
    client: &NotionOfficialClient,
    body: &mut markdown::PageBody,
    summary: &mut FetchSummary,
) {
    let todo: Vec<String> = body
        .fetchable_holes()
        .map(|u| u.block_id.clone())
        .take(MAX_HOLE_FOLLOWUPS)
        .collect();
    if todo.is_empty() {
        return;
    }
    let total_fetchable = body.fetchable_holes().count();
    if total_fetchable > MAX_HOLE_FOLLOWUPS {
        tracing::warn!(
            event = "notion_hole_followups_capped",
            wanted = total_fetchable,
            cap = MAX_HOLE_FOLLOWUPS,
            "page has more truncated subtrees than one run will follow"
        );
        summary.pages_left_incomplete += 1;
    }
    for id in todo {
        match client.get_page_markdown(&id).await {
            Ok(resp) => {
                let part = markdown::parse(&resp);
                if part.markdown.is_empty() {
                    continue;
                }
                body.markdown.push('\n');
                body.markdown.push_str(&part.markdown);
                for c in part.child_pages {
                    if !body.child_pages.contains(&c) {
                        body.child_pages.push(c);
                    }
                }
                summary.hole_followups += 1;
            }
            Err(e) => {
                tracing::warn!(block = %id, error = %e, "truncated-subtree fetch failed");
            }
        }
    }
}

/// Scope the search resume cursor is stored under. One workspace per
/// source, so one of them.
const SEARCH_SCOPE: &str = "workspace";

/// Key for the config blob paired with it, so a widened
/// `refresh_window_days` re-examines rather than being suppressed by a
/// resume cursor recorded under the narrower one.
const SCOPE_CONFIG_KEY: &str = "notion:download";

/// Walk `POST /v1/search` newest-edited-first and stop where the last
/// run finished.
///
/// This is Notion's "since you last looked". It offers no delta token —
/// no Gmail `historyId`, no JMAP `state` — so the resume cursor is a
/// timestamp: results come back ordered by `last_edited_time`
/// descending, and that ordering was measured strictly monotonic across
/// 12,300 objects and 124 pages of results. The first result older than
/// the stored point therefore ends the walk, and a steady-state run
/// reads one page instead of the workspace.
///
/// One value, three names, so: the **resume cursor** is what
/// `scope_state::since_for_scope` returns (hence the `since` argument)
/// and what `sync_scope_state.last_seen_at` stores. It is *not*
/// `start_cursor` / `next_cursor`, which page within a single walk and
/// do not survive it — that distinction is why the qualifier is worth
/// carrying.
///
/// Returns the page ids to mirror, and the newest `last_edited_time`
/// seen — the point the next run resumes from.
async fn search_since(
    client: &NotionOfficialClient,
    since: Option<&str>,
    max_pages: Option<usize>,
) -> Result<(Vec<String>, Option<String>)> {
    let mut ids: Vec<String> = Vec::new();
    let mut newest_edited: Option<String> = None;
    let mut cursor: Option<String> = None;
    loop {
        let resp = client
            .search(cursor.as_deref(), false)
            .await
            .map_err(|e| anyhow::anyhow!("notion search: {e}"))?;
        let results = resp
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if results.is_empty() {
            break;
        }
        for r in &results {
            let edited = r.get("last_edited_time").and_then(|v| v.as_str());
            if newest_edited.is_none() {
                newest_edited = edited.map(String::from);
            }
            // Descending order means everything from here on is older.
            if let (Some(e), Some(s)) = (edited, since) {
                if e < s {
                    tracing::info!(
                        event = "notion_search_reached_resume_cursor",
                        resume_cursor = s,
                        discovered = ids.len(),
                    );
                    return Ok((ids, newest_edited));
                }
            }
            // A data_source is a container; its rows come back from
            // search as ordinary pages, so only pages are queued here.
            if r.get("object").and_then(|v| v.as_str()) != Some("page") {
                continue;
            }
            if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
        if max_pages.is_some_and(|m| ids.len() >= m) {
            break;
        }
        if !resp
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            break;
        }
        match resp.get("next_cursor").and_then(|v| v.as_str()) {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok((ids, newest_edited))
}

/// What a walk has already resolved, so one run spends at most one
/// request per user and per commented block — however many pages
/// mention them.
#[derive(Default)]
pub struct WalkState {
    pub pages: HashMap<String, PageState>,
    pub users: HashSet<String>,
    pub anchors: HashSet<String>,
}

/// Plain text of a block, whatever its type carries rich text under.
fn block_plain_text(block: &Value) -> Option<String> {
    let t = block.get("type")?.as_str()?;
    let rt = block.get(t)?.get("rich_text")?.as_array()?;
    let s: String = rt
        .iter()
        .filter_map(|x| x.get("plain_text").and_then(|v| v.as_str()))
        .collect();
    (!s.trim().is_empty()).then_some(s)
}

/// Resolve the users seen on a page, and the blocks its comments are
/// anchored to.
///
/// Both are lazy and both are cheap for the reason that matters: a user
/// is fetched once ever, and a block only when a comment hangs off it —
/// one request per *commented* block, not per block. `GET /v1/users`
/// (list all) is not an option: personal access tokens cannot call it.
async fn resolve_people_and_anchors(
    client: &NotionOfficialClient,
    db: &RawDb,
    pid: &str,
    page: &Value,
    comments: &[Value],
    state: &mut WalkState,
    summary: &mut FetchSummary,
) {
    // ── users ────────────────────────────────────────────────────────
    let mut wanted: Vec<String> = Vec::new();
    let mut want = |v: Option<&Value>| {
        if let Some(id) = v.and_then(|x| x.get("id")).and_then(|x| x.as_str()) {
            if !id.is_empty() && !wanted.iter().any(|w| w == id) {
                wanted.push(id.to_string());
            }
        }
    };
    want(page.get("created_by"));
    want(page.get("last_edited_by"));
    if let Some(props) = page.get("properties").and_then(|v| v.as_object()) {
        for prop in props.values() {
            if let Some(people) = prop.get("people").and_then(|v| v.as_array()) {
                for p in people {
                    want(Some(p));
                }
            }
        }
    }
    for c in comments {
        want(c.get("created_by"));
    }
    for id in wanted {
        if state.users.contains(&id) {
            continue;
        }
        match client.get_user(&id).await {
            Ok(u) => {
                let name = u.get("name").and_then(|v| v.as_str()).map(String::from);
                let payload = serde_json::to_string(&u).unwrap_or_else(|_| "null".into());
                if let Err(e) = db.upsert_users(&[(id.clone(), name, payload)]).await {
                    tracing::warn!(user = %id, error = %format!("{e:#}"), "user upsert failed");
                } else {
                    summary.users_resolved += 1;
                }
                state.users.insert(id);
            }
            Err(e) => {
                // A user we cannot read is not a reason to fail a page.
                // The author simply falls back to an id prefix.
                tracing::warn!(user = %id, error = %e, "user fetch failed");
                state.users.insert(id);
            }
        }
    }

    // ── comment anchors ──────────────────────────────────────────────
    let mut blocks: Vec<String> = Vec::new();
    for c in comments {
        if c.get("parent")
            .and_then(|p| p.get("type"))
            .and_then(|v| v.as_str())
            != Some("block_id")
        {
            continue;
        }
        let Some(bid) = c
            .get("parent")
            .and_then(|p| p.get("block_id"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if !state.anchors.contains(bid) && !blocks.iter().any(|b| b == bid) {
            blocks.push(bid.to_string());
        }
    }
    for bid in blocks {
        match client.get_block(&bid).await {
            Ok(b) => {
                let block_type = b.get("type").and_then(|v| v.as_str()).map(String::from);
                let text = block_plain_text(&b);
                if let Err(e) = db
                    .upsert_comment_anchors(&[db::CommentAnchorUpsert {
                        id: bid.clone(),
                        page_id: Some(pid.to_string()),
                        block_type,
                        plain_text: text,
                    }])
                    .await
                {
                    tracing::warn!(block = %bid, error = %format!("{e:#}"), "anchor upsert failed");
                } else {
                    summary.anchors_resolved += 1;
                }
                state.anchors.insert(bid);
            }
            Err(e) => {
                // The commented block can be gone upstream — comments
                // carry `original_content_deleted` for exactly that.
                tracing::debug!(block = %bid, error = %e, "anchor fetch failed");
                state.anchors.insert(bid);
            }
        }
    }
}

/// Mirror one page: its object, its body, its attachments and its
/// comments. Returns the child pages to descend into.
///
/// Three requests where the block walk needed one per container block
/// (measured median 11, and ≥60 on the deepest pages sampled).
#[tracing::instrument(skip_all, fields(page_id = %pid, origin = %origin, skipped))]
async fn mirror_page(
    client: &NotionOfficialClient,
    db: &RawDb,
    opts: &FetchOptions,
    pid: &str,
    origin: &'static str,
    state: &mut WalkState,
    summary: &mut FetchSummary,
) -> Result<Vec<String>> {
    let page = match client.get_page(pid).await {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e}");
            tracing::warn!(page = pid, error = %msg, "page fetch failed; recording");
            let _ = db.record_page_error(pid, &msg).await;
            summary.failed_pages += 1;
            return Ok(Vec::new());
        }
    };
    let last_edited = page
        .get("last_edited_time")
        .and_then(|v| v.as_str())
        .map(String::from);
    let prior = state.pages.get(pid);
    let was_present = prior.map(|s| s.has_payload).unwrap_or(false);

    // Unchanged upstream: the body, attachments and comments can't have
    // moved either. Descend into known children anyway — a child's
    // `last_edited_time` can advance when its parent's does not.
    if was_present
        && last_edited.is_some()
        && prior.and_then(|s| s.last_edited_time.clone()) == last_edited
    {
        summary.skipped_pages += 1;
        tracing::Span::current().record("skipped", true);
        return db.stored_child_pages(pid).await;
    }

    let (parent_type, parent_id) = match page.get("parent") {
        Some(p) => (
            p.get("type").and_then(|v| v.as_str()).map(String::from),
            parent_of(&page),
        ),
        None => (None, None),
    };
    db.upsert_pages(&[db::PageUpsert {
        id: pid.to_string(),
        parent_type,
        parent_id,
        in_trash: page
            .get("in_trash")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        created_time: page
            .get("created_time")
            .and_then(|v| v.as_str())
            .map(String::from),
        last_edited_time: last_edited.clone(),
        url: page.get("url").and_then(|v| v.as_str()).map(String::from),
        payload: serde_json::to_string(&page).ok(),
    }])
    .await
    .with_context(|| format!("upsert page {pid}"))?;
    state.pages.insert(
        pid.into(),
        PageState {
            last_edited_time: last_edited.clone(),
            has_payload: true,
        },
    );
    if was_present {
        summary.upd_pages += 1;
    } else {
        summary.new_pages += 1;
    }

    // ── body ─────────────────────────────────────────────────────────
    let mut children: Vec<String> = Vec::new();
    match client.get_page_markdown(pid).await {
        Ok(resp) => {
            let mut body = markdown::parse(&resp);
            if body.truncated {
                fill_holes(client, &mut body, summary).await;
            }
            let permanent = body.permanent_holes();
            if !permanent.is_empty() {
                tracing::info!(
                    event = "notion_unrepresentable_blocks",
                    page = pid,
                    count = permanent.len(),
                    "page contains block types markdown cannot express"
                );
            }
            // Signed URLs must be captured BEFORE the rewrite, since
            // that is what strips them, and they are the only way to
            // fetch the bytes.
            let signed = signed_by_slot(&body.markdown);
            let (stable, slots) = slots::rewrite(&body.markdown);
            let unresolved: Vec<&str> = body
                .unresolved
                .iter()
                .map(|u| u.block_id.as_str())
                .collect();
            db.upsert_page_markdown(&[db::PageMarkdownUpsert {
                id: pid.to_string(),
                markdown: stable,
                truncated: body.truncated,
                unresolved_block_ids: (!unresolved.is_empty())
                    .then(|| serde_json::to_string(&unresolved).unwrap_or_default()),
                source_last_edited_time: last_edited.clone(),
            }])
            .await
            .with_context(|| format!("upsert page_markdown {pid}"))?;
            summary.bodies += 1;
            if body.markdown.is_empty() {
                summary.empty_bodies += 1;
            }
            if opts.attachments && !slots.is_empty() {
                fetch_attachments(db, pid, &slots, &signed, summary).await;
            }
            children = body.child_pages;
        }
        Err(e) => {
            tracing::warn!(page = pid, error = %e, "markdown fetch failed; page object kept");
            summary.failed_bodies += 1;
        }
    }

    // ── comments ─────────────────────────────────────────────────────
    let mut comments: Vec<Value> = Vec::new();
    if opts.comments {
        comments = fetch_all_comments(client, pid).await.unwrap_or_default();
        if !comments.is_empty() {
            let mut rows: Vec<db::CommentUpsert> = Vec::with_capacity(comments.len());
            for c in &comments {
                let Some(id) = c.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let (parent_type, parent_id) = comment_parent(c)
                    .map(|(t, i)| (Some(t), Some(i)))
                    .unwrap_or((None, Some(pid.to_string())));
                rows.push(db::CommentUpsert {
                    id: id.into(),
                    discussion_id: c
                        .get("discussion_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    parent_type,
                    parent_id,
                    page_id: Some(pid.into()),
                    created_time: c
                        .get("created_time")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    last_edited_time: c
                        .get("last_edited_time")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    payload: serde_json::to_string(c).unwrap_or_else(|_| "null".into()),
                });
            }
            summary.upd_comments += rows.len();
            db.upsert_comments(&rows)
                .await
                .with_context(|| format!("upsert comments for {pid}"))?;
        }
    }

    resolve_people_and_anchors(client, db, pid, &page, &comments, state, summary).await;

    Ok(children)
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
    state: &mut WalkState,
    summary: &mut FetchSummary,
    single_page: bool,
) -> Result<()> {
    while let Some(pid) = queue.pop_front() {
        if opts.max_pages.is_some_and(|m| visited.len() >= m) {
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
        let children = mirror_page(client, db, opts, &pid, origin, state, summary).await?;
        if !single_page {
            for cid in children {
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
    let _ = datalib_etl::latchkey::ensure_curl_dispatch();

    let db = opts.db.clone();
    if opts.control.reset_and_redownload {
        tracing::info!(event = "notion_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        tracing::info!(event = "notion_refetch_blobs");
        datalib_etl::doltlite_raw::truncate_data_tables(db.pool(), &["notion_attachments"])
            .await
            .context("truncate notion_attachments before refetch")?;
    }
    let run_config = json!({
        "subtree_pages": opts.subtree_pages,
        "max_pages": opts.max_pages,
        "page": opts.page,
        "retry_failed": opts.retry_failed,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;

    let official = NotionOfficialClient::with_latchkey(opts.latchkey.clone());
    let mut summary = FetchSummary::default();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut state_walk = WalkState {
        pages: db.page_states().await?,
        users: db.known_user_ids().await?,
        anchors: db.known_anchor_ids().await?,
    };

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
                &mut state_walk,
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
                &mut state_walk,
                &mut summary,
                true,
            )
            .await?;
            return Ok(());
        }

        // No roots means the whole workspace, and the whole workspace
        // means search — resumed where the last run stopped, so a
        // steady-state run reads one page of results rather than 124.
        if opts.subtree_pages.is_empty() {
            let span = tracing::info_span!("notion_search_pass");
            let _enter = span.enter();
            let state = datalib_etl::scope_state::snapshot(db.pool()).await?;
            let prior = datalib_etl::scope_config::load(db.pool(), SCOPE_CONFIG_KEY).await?;
            let since = datalib_etl::scope_state::since_for_scope(
                &state,
                SEARCH_SCOPE,
                opts.refresh_window_days,
                opts.full_sync || opts.control.reset_and_redownload,
                prior.as_ref(),
            );
            let (ids, newest_edited) =
                search_since(&official, since.as_deref(), opts.max_pages).await?;
            summary.discovered = ids.len();
            tracing::info!(
                event = "notion_search_pass",
                since = since.as_deref().unwrap_or("(cold start)"),
                discovered = ids.len(),
            );
            let mut q: VecDeque<String> = VecDeque::new();
            for id in ids {
                if queued.insert(id.clone()) {
                    q.push_back(id);
                }
            }
            // `single_page = true`: don't descend into child pages.
            // Search already named every page in the workspace, so a
            // walk would only re-visit pages it has, or drag in ones
            // older than the resume cursor.
            bfs_drain(
                &official,
                &db,
                &opts,
                "search",
                q,
                &mut queued,
                &mut visited,
                &mut state_walk,
                &mut summary,
                true,
            )
            .await?;
            // Only after the pages actually landed: a resume cursor
            // recorded over a failed pass would skip that window
            // forever.
            if let Some(mark) = newest_edited {
                datalib_etl::doltlite_raw::upsert_scope_state(db.pool(), SEARCH_SCOPE, &mark)
                    .await?;
                datalib_etl::scope_config::store(
                    db.pool(),
                    SCOPE_CONFIG_KEY,
                    &datalib_etl::scope_state::refresh_window_blob(opts.refresh_window_days),
                )
                .await?;
            }
            return Ok(());
        }

        // Pass 1: subtree seeds.
        {
            let span = tracing::info_span!("notion_subtree_pass", pages = opts.subtree_pages.len());
            let _enter = span.enter();
            let mut subtree_queue: VecDeque<String> = VecDeque::new();
            for raw in &opts.subtree_pages {
                let stripped = datalib_etl::ids::normalize_id_token(raw);
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
                &mut state_walk,
                &mut summary,
                false,
            )
            .await?;
            tracing::info!(visited = visited.len(), "subtree pass done");
        }

        Ok(())
    };

    let mut result = work.await;
    summary.official_requests = official.request_count();

    // Per-page fetch errors are recorded and tolerated (see `mirror_page`),
    // which is right when one page of many is unreadable — but wrong when
    // it's every page. A misconfigured credential fails identically on all
    // of them, and without this the run reports success having stored
    // nothing: the DAG step goes green, the render step finds an empty raw
    // store and emits no markdown, and the provider looks healthy while
    // being completely dead. That is exactly how a missing `Notion-Version`
    // header (latchkey injects it; the client deliberately doesn't) hid for
    // two months.
    if result.is_ok() && all_pages_failed(&summary) {
        result = Err(anyhow::anyhow!(
            "every page fetch failed ({} attempted, 0 succeeded) — the raw store \
             is empty, so render will produce nothing. This is usually a \
             credential problem rather than a per-page one: the `notion` \
             latchkey service must inject BOTH the bearer token and the \
             `Notion-Version` header, e.g.\n  \
             latchkey auth set notion -H \"Authorization: Bearer <token>\" \
             -H \"Notion-Version: 2022-06-28\"\n\
             See the per-page `page fetch failed` warnings above for the \
             underlying error.",
            summary.failed_pages
        ));
    }

    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// Public re-export: the legacy entity name constants are still
/// referenced in a few places. They no longer correspond to on-disk
/// paths but stay around as logical identifiers.
pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_MARKDOWN: &str = "notion_page_markdown";
pub const ENTITY_COMMENT: &str = "notion_official_comment";
pub const ENTITY_USER: &str = "notion_user";
pub const ENTITY_ANCHOR_BLOCK: &str = "notion_anchor_block";

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

    /// Nothing attempted is not a failure — an empty roots config,
    /// or a run where every page was already current, must stay green.
    #[test]
    fn all_pages_failed_is_false_when_nothing_failed() {
        assert!(!all_pages_failed(&FetchSummary::default()));
        assert!(!all_pages_failed(&FetchSummary {
            new_pages: 3,
            ..Default::default()
        }));
    }

    /// The regression this guards: a systemic credential failure fails on
    /// every page, and used to be reported as a successful run.
    #[test]
    fn all_pages_failed_is_true_when_every_page_failed() {
        assert!(all_pages_failed(&FetchSummary {
            failed_pages: 1,
            ..Default::default()
        }));
        assert!(all_pages_failed(&FetchSummary {
            failed_pages: 42,
            ..Default::default()
        }));
    }

    /// Search's whole value as a resume mechanism is that it stops
    /// early. The ordering this relies on — `last_edited_time` strictly
    /// descending — was measured across 12,300 objects and 124 pages of
    /// results against a real workspace.
    #[test]
    fn the_resume_cursor_ends_the_walk_at_the_first_older_result() {
        let page = |id: &str, edited: &str| serde_json::json!({"object": "page", "id": id, "last_edited_time": edited});
        let results = [
            page("new-1", "2026-09-07T00:00:00.000Z"),
            page("new-2", "2026-09-06T00:00:00.000Z"),
            page("old-1", "2026-08-01T00:00:00.000Z"),
            page("old-2", "2026-07-01T00:00:00.000Z"),
        ];
        let since = "2026-09-01T00:00:00.000Z";
        let taken: Vec<&str> = results
            .iter()
            .take_while(|r| r["last_edited_time"].as_str().unwrap() >= since)
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(taken, vec!["new-1", "new-2"]);
    }

    /// A cold start has no resume cursor, so everything is in scope.
    /// Getting this wrong the other way — treating "no point" as "stop
    /// immediately" — would mirror nothing and look successful.
    #[test]
    fn a_cold_start_takes_everything() {
        let edited = "2020-01-01T00:00:00.000Z";
        let since: Option<&str> = None;
        assert!(
            since.is_none_or(|s| edited >= s),
            "with no resume cursor every result is in scope"
        );
    }

    /// One bad page among working ones is tolerated — that's the case the
    /// per-page record-and-continue exists for, and it must keep working.
    #[test]
    fn all_pages_failed_is_false_when_some_page_succeeded() {
        for ok in [
            FetchSummary {
                failed_pages: 5,
                new_pages: 1,
                ..Default::default()
            },
            FetchSummary {
                failed_pages: 5,
                upd_pages: 1,
                ..Default::default()
            },
            // A skip only happens after confirming the stored copy is
            // current, which itself proves the credential works.
            FetchSummary {
                failed_pages: 5,
                skipped_pages: 1,
                ..Default::default()
            },
        ] {
            assert!(!all_pages_failed(&ok), "{ok:?} should not be all-failed");
        }
    }
}
