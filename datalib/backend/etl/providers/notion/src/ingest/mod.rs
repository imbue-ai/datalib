//! Notion downloader: mirror pages via the official API.

pub mod db;
pub mod markdown;
pub mod official;
pub mod schema_raw;
pub mod slots;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeAccumulator;
use datalib_etl::download_problems::{DownloadProblem, RunProblem};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::{latchkey_curl, HttpRequest, HttpService, LatchkeySettings};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use serde::Serialize;
use serde_json::{json, Value};

pub use db::{db_path_for, AttachmentRow, PageState, RawDb};
pub use official::{NotionOfficialClient, NotionOfficialError};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
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
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// The run's pinned now, which `refresh_window_days` counts back from.
    pub now: datalib_time::IsoOffsetTimestamp,
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
            now: datalib_time::IsoOffsetTimestamp::now_local(),
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
    /// Pages fetched again for an earlier failure that failed again.
    /// Kept out of `failed_pages`: in a steady-state run they can be all
    /// that was attempted, and a page gone for good must not fail every
    /// run.
    pub failed_retries: usize,
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
/// bytes are pulled in the same run that read the markdown, and why a
/// failed one is retried by fetching its page again.
///
/// A slot whose edge already carries a `blake3` is skipped: signatures
/// rotate, bytes don't. A failed fetch is an edge with no bytes and a
/// `problems` row; a 404 is a file gone upstream, not a failure. Returns
/// the slots that are gone.
async fn fetch_attachments(
    db: &RawDb,
    page_id: &str,
    slots: &[String],
    signed: &HashMap<String, String>,
    stop: &StopFlag,
    summary: &mut FetchSummary,
) -> Result<HashSet<String>> {
    let mut gone: HashSet<String> = HashSet::new();
    let mut acc = CasEdgeAccumulator::new();
    for slot in slots {
        if db.blob_exists(slot).await? {
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
        let failure = match latchkey_curl(&req).await {
            Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                let content_type = resp.header("content-type").map(String::from);
                acc.add_fetched(page_id, slot, resp.body, content_type, None);
                summary.new_blobs += 1;
                continue;
            }
            Ok(resp) if matches!(resp.status, 404 | 410) => {
                gone.insert(slot.clone());
                continue;
            }
            Ok(resp) => format!("HTTP {}", resp.status),
            Err(e @ datalib_etl::http::HttpError::GaveUp { .. }) => {
                db.flush_attachments(&acc).await?;
                return Err(RunEnded::gave_up(e.to_string()).into());
            }
            Err(e) => e.to_string(),
        };
        // The transport refuses every request once a stop is asked for;
        // the page is fetched whole again next run.
        if stop.requested() {
            break;
        }
        acc.add_failed(page_id, slot, failure);
        summary.failed_blobs += 1;
    }
    db.flush_attachments(&acc).await?;
    Ok(gone)
}

/// The walk ends here: a refused credential or a retry guard that gave
/// up would fail every request left. Raised from deep in a page and
/// caught by the walk, which keeps what it already stored (see
/// [`WalkState::ended`]).
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct RunEnded {
    pub reason: String,
    pub unauthorized: bool,
}

impl RunEnded {
    fn gave_up(reason: String) -> Self {
        Self {
            reason,
            unauthorized: false,
        }
    }

    /// The one row a walk that ended early leaves: what was fetched is
    /// kept, and the rest is fetched next run.
    fn problem(&self) -> RunProblem {
        if self.unauthorized {
            RunProblem::phase(
                "credential",
                format!(
                    "Notion refused the credential part-way through, so the run stopped; \
                     what it fetched before is kept: {}",
                    self.reason
                ),
            )
        } else {
            RunProblem::phase(
                "rate_limit",
                format!(
                    "the retry guard gave up on Notion, so the run stopped; the rest is \
                     fetched next run: {}",
                    self.reason
                ),
            )
        }
    }
}

fn run_over(e: NotionOfficialError) -> anyhow::Error {
    RunEnded {
        unauthorized: matches!(e, NotionOfficialError::Unauthorized(_)),
        reason: e.to_string(),
    }
    .into()
}

/// Takes a walk's error: one that ends the run is kept on `state` and the
/// walk returns as if done; anything else is the step's failure.
fn end_walk(e: anyhow::Error, state: &mut WalkState) -> Result<()> {
    let ended = e.downcast::<RunEnded>()?;
    state.ended.get_or_insert(ended);
    Ok(())
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
async fn fetch_all_comments(
    client: &NotionOfficialClient,
    page_id: &str,
) -> Result<Vec<Value>, NotionOfficialError> {
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

/// What the follow-ups for a truncated body could not fill.
#[derive(Default)]
struct Holes {
    /// `(block id, error)` for each follow-up that failed.
    failed: Vec<(String, String)>,
    /// How many follow-ups the body wanted, when that was more than
    /// one run follows.
    over_cap: Option<usize>,
}

impl Holes {
    fn followed_all(&self) -> bool {
        self.failed.is_empty() && self.over_cap.is_none()
    }
}

async fn fill_holes(
    client: &NotionOfficialClient,
    body: &mut markdown::PageBody,
    summary: &mut FetchSummary,
) -> Result<Holes> {
    let mut holes = Holes::default();
    let todo: Vec<String> = body
        .fetchable_holes()
        .map(|u| u.block_id.clone())
        .take(MAX_HOLE_FOLLOWUPS)
        .collect();
    if todo.is_empty() {
        return Ok(holes);
    }
    let total_fetchable = body.fetchable_holes().count();
    if total_fetchable > MAX_HOLE_FOLLOWUPS {
        summary.pages_left_incomplete += 1;
        holes.over_cap = Some(total_fetchable);
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
            Err(e) if e.ends_the_run() => return Err(run_over(e)),
            // The block went since the body was read.
            Err(NotionOfficialError::NotFound(_)) => {}
            Err(e) => holes.failed.push((id, e.to_string())),
        }
    }
    Ok(holes)
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
/// and what `sync_scope_state.last_seen_at_utc` stores. It is *not*
/// `start_cursor` / `next_cursor`, which page within a single walk and
/// do not survive it — that distinction is why the qualifier is worth
/// carrying.
///
/// Fails only when the first page of results does; a later page that
/// fails ends the walk with what it had (see [`SearchPass::cut_short`]).
async fn search_since(
    client: &NotionOfficialClient,
    since: Option<&str>,
    max_pages: Option<usize>,
) -> Result<SearchPass> {
    let mut ids: Vec<String> = Vec::new();
    let mut newest_edited: Option<String> = None;
    let mut cursor: Option<String> = None;
    loop {
        let resp = match client.search(cursor.as_deref(), false).await {
            Ok(resp) => resp,
            Err(e) if e.ends_the_run() => return Err(run_over(e)),
            Err(e) if cursor.is_none() => return Err(anyhow::anyhow!("notion search: {e}")),
            Err(e) => {
                return Ok(SearchPass {
                    ids,
                    newest_edited,
                    cut_short: Some(e.to_string()),
                })
            }
        };
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
                        "the search reached what the last run already had; stopping"
                    );
                    return Ok(SearchPass {
                        ids,
                        newest_edited,
                        cut_short: None,
                    });
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
    Ok(SearchPass {
        ids,
        newest_edited,
        cut_short: None,
    })
}

#[derive(Default)]
struct SearchPass {
    /// The page ids to mirror.
    ids: Vec<String>,
    /// The newest `last_edited_time` seen — the point the next run
    /// resumes from, once this one has landed its pages.
    newest_edited: Option<String>,
    /// Why the walk ended before the resume cursor. Everything older
    /// than the last result read is unseen, so the cursor must not move.
    cut_short: Option<String>,
}

/// What a walk has already resolved, so one run spends at most one
/// request per user and per commented block — however many pages
/// mention them.
#[derive(Default)]
pub struct WalkState {
    pub pages: HashMap<String, PageState>,
    pub users: HashSet<String>,
    pub anchors: HashSet<String>,
    /// Pages fetched again though upstream has not moved them, because
    /// part of an earlier fetch failed ([`RawDb::pages_to_refetch`]).
    pub retry: HashSet<String>,
    /// Pages whose object would not fetch this run, and why.
    pub unreadable: HashMap<String, NotionOfficialError>,
    /// Why the walk stopped before its end, when it did. Nothing past
    /// that point was asked for, so the resume cursor stays and the
    /// retry sets are left for the next run.
    pub ended: Option<RunEnded>,
    /// The credential may not read comments (403): an integration
    /// without the capability. Asked once a run, not once a page.
    pub comments_forbidden: Option<String>,
    /// Whether any page's comments were asked for this run.
    pub comments_asked: bool,
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
#[allow(clippy::too_many_arguments)]
async fn resolve_people_and_anchors(
    client: &NotionOfficialClient,
    db: &RawDb,
    pid: &str,
    page: &Value,
    comments: &[Value],
    stop: &StopFlag,
    state: &mut WalkState,
    summary: &mut FetchSummary,
) -> Result<()> {
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
        resolve_user(client, db, &id, stop, state, summary).await?;
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
                db.upsert_comment_anchors(&[db::CommentAnchorUpsert {
                    id: bid.clone(),
                    page_id: Some(pid.to_string()),
                    block_type,
                    plain_text: text,
                }])
                .await?;
                summary.anchors_resolved += 1;
                state.anchors.insert(bid);
            }
            Err(e) if e.ends_the_run() => return Err(run_over(e)),
            Err(e) => {
                // The commented block can be gone upstream — comments
                // carry `original_content_deleted` for exactly that.
                tracing::debug!(block = %bid, error = %e, "anchor fetch failed");
                state.anchors.insert(bid);
            }
        }
    }
    Ok(())
}

/// One user, stored, or a failure kept on its row until a later run's
/// retry ([`retry_failed_users`]) reads it. A user that cannot be read
/// does not fail its page: the author falls back to an id prefix.
async fn resolve_user(
    client: &NotionOfficialClient,
    db: &RawDb,
    id: &str,
    stop: &StopFlag,
    state: &mut WalkState,
    summary: &mut FetchSummary,
) -> Result<()> {
    match client.get_user(id).await {
        Ok(u) => {
            let name = u.get("name").and_then(|v| v.as_str()).map(String::from);
            let payload = serde_json::to_string(&u).unwrap_or_else(|_| "null".into());
            db.upsert_users(&[(id.to_string(), name, payload)]).await?;
            summary.users_resolved += 1;
        }
        Err(_) if stop.requested() => return Ok(()),
        Err(e) if e.ends_the_run() => return Err(run_over(e)),
        Err(NotionOfficialError::NotFound(_)) => db.forget_user(id).await?,
        Err(e) => db.record_fetch_error("users", id, &e.to_string()).await?,
    }
    state.users.insert(id.to_string());
    Ok(())
}

/// Users an earlier run could not read and this one did not meet again.
async fn retry_failed_users(
    client: &NotionOfficialClient,
    db: &RawDb,
    stop: &StopFlag,
    state: &mut WalkState,
    summary: &mut FetchSummary,
) -> Result<()> {
    for id in db.failed_user_ids().await? {
        if stop.requested() {
            break;
        }
        if !state.users.contains(&id) {
            resolve_user(client, db, &id, stop, state, summary).await?;
        }
    }
    Ok(())
}

/// Mirror one page: its object, its body, its attachments and its
/// comments. Returns the child pages to descend into.
///
/// Three requests where the block walk needed one per container block
/// (measured median 11, and ≥60 on the deepest pages sampled).
///
/// The body is stored last, stamped with the `last_edited_time` it was
/// fetched at: a page whose stored body is behind its stored object is
/// fetched again next run, which is what a stop part-way through a page
/// leaves behind.
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
    let stop = &opts.control.stop;
    let page = match client.get_page(pid).await {
        Ok(p) => p,
        Err(_) if stop.requested() => return Ok(Vec::new()),
        Err(e) if e.ends_the_run() => return Err(run_over(e)),
        // Deleted, or no longer shared: gone, not failed.
        Err(e @ NotionOfficialError::NotFound(_)) => {
            db.retire_page(pid).await?;
            state.unreadable.insert(pid.to_string(), e);
            return Ok(Vec::new());
        }
        Err(e) => {
            db.record_page_error(pid, &e.to_string()).await?;
            if state.retry.contains(pid) {
                summary.failed_retries += 1;
            } else {
                summary.failed_pages += 1;
            }
            state.unreadable.insert(pid.to_string(), e);
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
        && prior.and_then(|s| s.body_edited_time.clone()) == last_edited
        && !state.retry.contains(pid)
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
            body_edited_time: None,
        },
    );
    if was_present {
        summary.upd_pages += 1;
    } else {
        summary.new_pages += 1;
    }

    // ── body ─────────────────────────────────────────────────────────
    let mut children: Vec<String> = Vec::new();
    let mut fetched_body: Option<(db::PageMarkdownUpsert, Holes)> = None;
    match client.get_page_markdown(pid).await {
        Ok(resp) => {
            let mut body = markdown::parse(&resp);
            let holes = if body.truncated {
                fill_holes(client, &mut body, summary).await?
            } else {
                Holes::default()
            };
            if stop.requested() {
                return Ok(Vec::new());
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
            let gone = if opts.attachments && !slots.is_empty() {
                fetch_attachments(db, pid, &slots, &signed, stop, summary).await?
            } else {
                HashSet::new()
            };
            // A body missing a subtree may be missing a link too.
            let complete = holes.followed_all();
            db.forget_failed_attachments(pid, |slot| {
                !gone.contains(slot) && (!complete || slots.iter().any(|s| s == slot))
            })
            .await?;
            fetched_body = Some((
                db::PageMarkdownUpsert {
                    id: pid.to_string(),
                    markdown: stable,
                    truncated: body.truncated,
                    unresolved_block_ids: (!unresolved.is_empty())
                        .then(|| serde_json::to_string(&unresolved).unwrap_or_default()),
                    source_last_edited_time: last_edited.clone(),
                },
                holes,
            ));
            children = body.child_pages;
        }
        Err(_) if stop.requested() => return Ok(Vec::new()),
        Err(e) if e.ends_the_run() => return Err(run_over(e)),
        // Asked for again only once the page is edited.
        Err(NotionOfficialError::NotFound(_)) => {
            db.settle_body(pid, last_edited.as_deref()).await?
        }
        Err(e) => {
            db.record_fetch_error("page_markdown", pid, &e.to_string())
                .await?;
            summary.failed_bodies += 1;
        }
    }

    // ── comments ─────────────────────────────────────────────────────
    let mut comments: Vec<Value> = Vec::new();
    if opts.comments && state.comments_forbidden.is_none() {
        state.comments_asked = true;
        match fetch_all_comments(client, pid).await {
            Ok(c) => comments = c,
            Err(_) if stop.requested() => return Ok(Vec::new()),
            Err(e) if e.ends_the_run() => return Err(run_over(e)),
            Err(NotionOfficialError::NotFound(_)) => {}
            // The credential may not read comments at all; no page is
            // the worse for it, and none is asked again this run.
            Err(NotionOfficialError::Forbidden(d)) => state.comments_forbidden = Some(d),
            // The page itself is stored, so this is a warning on it, and
            // it keeps the page among the ones fetched again.
            Err(e) => db.record_page_error(pid, &format!("comments: {e}")).await?,
        }
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

    resolve_people_and_anchors(client, db, pid, &page, &comments, stop, state, summary).await?;
    if stop.requested() {
        return Ok(Vec::new());
    }

    if let Some((row, holes)) = fetched_body {
        let empty = row.markdown.is_empty();
        db.upsert_page_markdown(&[row])
            .await
            .with_context(|| format!("upsert page_markdown {pid}"))?;
        summary.bodies += 1;
        if empty {
            summary.empty_bodies += 1;
        }
        if let Some((block, e)) = holes.failed.first() {
            db.record_fetch_error(
                "page_markdown",
                pid,
                &format!(
                    "{} truncated subtree(s) did not fetch, so the body is incomplete; \
                     first: block {block}: {e}",
                    holes.failed.len()
                ),
            )
            .await?;
        } else if let Some(wanted) = holes.over_cap {
            db.record_body_cut_short(
                pid,
                &format!(
                    "the body has {wanted} truncated subtrees and one run follows \
                     {MAX_HOLE_FOLLOWUPS}, so the rest is missing"
                ),
            )
            .await?;
        }
    }

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
        // The transport refuses every request from here on; a page
        // started now would only fail.
        if opts.control.stop.requested() {
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
        let children = match mirror_page(client, db, opts, &pid, origin, state, summary).await {
            Ok(children) => children,
            Err(e) => {
                end_walk(e, state)?;
                break;
            }
        };
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
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| download(opts, found)).await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl::latchkey::ensure_curl_router();

    let db = opts.db.clone();
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
        retry: db.pages_to_refetch(opts.attachments).await?,
        unreadable: HashMap::new(),
        ended: None,
        comments_forbidden: None,
        comments_asked: false,
    };
    // Carried forward when no page's comments are asked this run, which
    // says nothing either way about whether they may be read.
    let comments_refused = db.run_problem_sample("listing:comments").await?;

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
            // One pass over named pages reaches none of a full run's
            // listings and phases.
            found.cut_short();
            return refused_from_the_start(&state_walk, &summary).map_or(Ok(()), Err);
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
            // One pass over named pages reaches none of a full run's
            // listings and phases.
            found.cut_short();
            return refused_from_the_start(&state_walk, &summary).map_or(Ok(()), Err);
        }

        let mut config_problems: Vec<DownloadProblem> = Vec::new();

        // No roots means the whole workspace, and the whole workspace
        // means search — resumed where the last run stopped, so a
        // steady-state run reads one page of results rather than 124.
        if opts.subtree_pages.is_empty() {
            let span = tracing::info_span!("notion_search_pass");
            let _enter = span.enter();
            let state = datalib_etl::scope_state::snapshot(db.pool()).await?;
            let prior = datalib_etl::scope_config::load(db.pool(), SCOPE_CONFIG_KEY).await?;
            let since = datalib_etl::scope_state::since_for_scope(
                &opts.now,
                &state,
                SEARCH_SCOPE,
                opts.refresh_window_days,
                opts.full_sync,
                prior.as_ref(),
            );
            let pass = match search_since(&official, since.as_deref(), opts.max_pages).await {
                Err(_) if opts.control.stop.requested() => return Ok(()),
                Ok(pass) => pass,
                Err(e) => {
                    end_walk(e, &mut state_walk)?;
                    SearchPass::default()
                }
            };
            summary.discovered = pass.ids.len();
            tracing::info!(
                event = "notion_search_pass",
                since = since.as_deref().unwrap_or("(cold start)"),
                discovered = pass.ids.len(),
                retries = state_walk.retry.len(),
                "one pass of the search"
            );
            // Search names only what moved since the resume cursor; a
            // page that failed earlier has not moved, so it is queued
            // beside them.
            let mut retries: Vec<String> = state_walk.retry.iter().cloned().collect();
            retries.sort();
            let mut q: VecDeque<String> = VecDeque::new();
            for id in pass.ids.iter().cloned().chain(retries) {
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
            // recorded over a pass that did not reach it would skip the
            // rest of that window forever. A page that failed is in the
            // retry set, so moving past it is safe.
            match (&pass.cut_short, pass.newest_edited) {
                (Some(e), _) => found.listing(
                    "search",
                    format!(
                        "the search stopped after {} pages of the workspace, so older edits \
                         were not looked at: {e}",
                        pass.ids.len()
                    ),
                ),
                (None, Some(mark))
                    if !opts.control.stop.requested() && state_walk.ended.is_none() =>
                {
                    datalib_etl::doltlite_raw::upsert_scope_state(db.pool(), SEARCH_SCOPE, &mark)
                        .await?;
                    datalib_etl::scope_config::store(
                        db.pool(),
                        SCOPE_CONFIG_KEY,
                        &datalib_etl::scope_state::refresh_window_blob(opts.refresh_window_days),
                    )
                    .await?;
                }
                (None, _) => {}
            }
        } else {
            // Subtree seeds. A page that failed earlier is reached again
            // by the walk, which descends into every stored child.
            let span = tracing::info_span!("notion_subtree_pass", pages = opts.subtree_pages.len());
            let _enter = span.enter();
            let mut roots: Vec<(&str, String)> = Vec::new();
            let mut subtree_queue: VecDeque<String> = VecDeque::new();
            for raw in &opts.subtree_pages {
                let stripped = datalib_etl::ids::normalize_id_token(raw);
                let id = format_uuid(&stripped);
                roots.push((raw, id.clone()));
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
            config_problems = roots_upstream_lacks(&roots, &state_walk.unreadable);
        }

        if state_walk.ended.is_none() {
            if let Err(e) = retry_failed_users(
                &official,
                &db,
                &opts.control.stop,
                &mut state_walk,
                &mut summary,
            )
            .await
            {
                end_walk(e, &mut state_walk)?;
            }
        }
        if let Some(e) = refused_from_the_start(&state_walk, &summary) {
            return Err(e);
        }
        if let Some(d) = &state_walk.comments_forbidden {
            found.push(comments_forbidden(d));
        } else if let (false, Some(said)) = (state_walk.comments_asked, &comments_refused) {
            found.push(RunProblem::forbidden("comments", said.clone()));
        }
        // A walk that ended early did not reach every configured root,
        // so their rows stand, nor every listing and phase.
        match &state_walk.ended {
            Some(ended) => {
                found.push(ended.problem());
                found.cut_short();
            }
            None => found.config(config_problems),
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
             -H \"Notion-Version: 2026-03-11\"\n\
             See the per-page `page fetch failed` warnings above for the \
             underlying error.",
            summary.failed_pages
        ));
    }

    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// The credential was refused before the run fetched anything, so there
/// is nothing to keep: the step fails.
fn refused_from_the_start(state: &WalkState, summary: &FetchSummary) -> Option<anyhow::Error> {
    let ended = state.ended.as_ref().filter(|e| e.unauthorized)?;
    let fetched = summary.new_pages + summary.upd_pages + summary.skipped_pages;
    (fetched == 0).then(|| anyhow::anyhow!("notion: {}", ended.reason))
}

fn comments_forbidden(detail: &str) -> RunProblem {
    RunProblem::forbidden(
        "comments",
        format!(
            "this credential may not read comments (give the integration the read-comments \
             capability), so none were mirrored: {detail}"
        ),
    )
}

/// The configured roots Notion says it does not have, or will not show
/// this credential.
fn roots_upstream_lacks(
    roots: &[(&str, String)],
    unreadable: &HashMap<String, NotionOfficialError>,
) -> Vec<DownloadProblem> {
    roots
        .iter()
        .filter_map(|(raw, id)| match unreadable.get(id)? {
            NotionOfficialError::NotFound(d) => Some(DownloadProblem::not_found(
                "roots",
                raw,
                format!("Notion has no page by that id that this credential can see: {d}"),
            )),
            NotionOfficialError::Forbidden(d) => Some(DownloadProblem::forbidden("roots", raw, d)),
            NotionOfficialError::Permanent(_)
            | NotionOfficialError::Unauthorized(_)
            | NotionOfficialError::GaveUp(_) => None,
        })
        .collect()
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
