//! Port of `_notion_rows` from `src/ingest/grid_rows.py`. Emits:
//!
//! - One `Notion Page` row per page.
//! - One `Notion Comment Thread` row per discussion.
//! - One `Notion Comment` row per individual comment.
//!
//! For sidecar emission we group rows per *document*. There are two
//! kinds of documents on the Notion side:
//!
//! - A page → sidecar `<page_uuid>.grid_rows.json` carrying the single
//!   `Notion Page` row.
//! - A discussion → sidecar `<discussion_uuid>.grid_rows.json` carrying
//!   the thread row + its comment rows.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::time::Instant;

use anyhow::Result;
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::ParsedNotionOfficial;
use super::render::{
    notion_thread_url, notion_url, page_qmd_path_rel, slugify, thread_qmd_path_rel, thread_snippet,
};

pub const RENDER_VERSION: u32 = 1;

fn page_title_from(page: &Value) -> String {
    let props = page.get("properties");
    if let Some(obj) = props.and_then(|v| v.as_object()) {
        for prop in obj.values() {
            if prop.get("type").and_then(|v| v.as_str()) == Some("title") {
                let rt = prop.get("title").and_then(|v| v.as_array());
                let plain: String = rt
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.get("plain_text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                return if plain.is_empty() {
                    "(untitled)".into()
                } else {
                    plain
                };
            }
        }
    }
    "(untitled)".into()
}

fn rich_text_plain(rt: Option<&Value>) -> String {
    rt.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("plain_text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn comment_text_plain(c: &Value) -> String {
    rich_text_plain(c.get("rich_text"))
}

fn build_page_titles(pages: &[Value], blocks: &[Value]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for p in pages {
        let id = p
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !id.is_empty() {
            out.insert(id, page_title_from(p));
        }
    }
    for b in blocks {
        if b.get("type").and_then(|v| v.as_str()) != Some("child_page") {
            continue;
        }
        let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let title = b
            .get("child_page")
            .and_then(|v| v.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.entry(id.into()).or_insert(title);
    }
    out
}

fn block_to_page_id(blocks: &[Value]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for b in blocks {
        let parent = b.get("parent");
        if parent.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("page_id") {
            let bid = b
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pid = parent
                .and_then(|v| v.get("page_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !bid.is_empty() && !pid.is_empty() {
                out.insert(bid, pid);
            }
        }
    }
    out
}

fn resolve_comment_page_id(
    comment: &Value,
    blocks: &[Value],
    block_owning_page: &HashMap<String, String>,
) -> Option<String> {
    let parent = comment.get("parent")?;
    let ptype = parent.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ptype == "page_id" {
        return parent
            .get("page_id")
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    if ptype == "block_id" {
        let bid = parent.get("block_id").and_then(|v| v.as_str())?;
        let mut block_parent: HashMap<String, String> = HashMap::new();
        for b in blocks {
            let par = b.get("parent");
            if par.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("block_id") {
                let id = b
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let pp = par
                    .and_then(|v| v.get("block_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !id.is_empty() && !pp.is_empty() {
                    block_parent.insert(id, pp);
                }
            }
        }
        let mut cur = Some(bid.to_string());
        let mut seen = std::collections::HashSet::new();
        while let Some(c) = cur {
            if !seen.insert(c.clone()) {
                break;
            }
            if let Some(p) = block_owning_page.get(&c) {
                return Some(p.clone());
            }
            cur = block_parent.get(&c).cloned();
        }
    }
    None
}

fn short_author(uid: &str, user_names: &HashMap<String, String>) -> Option<String> {
    if let Some(name) = user_names.get(uid) {
        if !name.is_empty() {
            return Some(name.clone());
        }
    }
    let s: String = uid.chars().take(8).collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn page_row(page: &Value, title: &str, user_names: &HashMap<String, String>) -> Result<GridRow> {
    let pid = page
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let when_ts: Option<String> = page
        .get("last_edited_time")
        .and_then(|v| v.as_str())
        .or_else(|| page.get("created_time").and_then(|v| v.as_str()))
        .map(str::to_string);
    let author_id = page
        .get("last_edited_by")
        .or_else(|| page.get("created_by"))
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    GridRow::builder()
        .uuid(pid.clone())
        .provider("notion")
        .kind("Notion Page")
        .source_label("Notion")
        .when_ts(when_ts)
        .author(short_author(author_id, user_names))
        .conversation_name(Some(title.to_string()))
        .conversation_uuid(pid.clone())
        .entire_chat(format!("/notion/page/{pid}"))
        .text(title.to_string())
        .qmd_path(Some(page_qmd_path_rel(&pid, title)))
        .source_url(Some(notion_url(&pid)))
        .notion_page_uuid(Some(pid.clone()))
        .markdown_uuid(Some(pid))
        .build()
        .map_err(anyhow::Error::from)
}

/// Rows for one discussion: the thread row + per-comment rows.
fn thread_rows(
    disc_id: &str,
    members_sorted: &[Value],
    page_id: &str,
    page_title: &str,
    parent_block_id: Option<&str>,
    user_names: &HashMap<String, String>,
) -> Result<Vec<GridRow>> {
    if members_sorted.is_empty() {
        return Ok(Vec::new());
    }
    let snippet = thread_snippet(&comment_text_plain(&members_sorted[0]));
    let thread_qmd = thread_qmd_path_rel(page_id, page_title, disc_id, &snippet);
    let thread_url = notion_thread_url(page_id, Some(disc_id), parent_block_id);
    let mut rows: Vec<GridRow> = Vec::new();
    let first = &members_sorted[0];
    let first_author_id = first
        .get("created_by")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let aggregated_text: String = members_sorted
        .iter()
        .map(comment_text_plain)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    rows.push(
        GridRow::builder()
            .uuid(disc_id)
            .provider("notion")
            .kind("Notion Comment Thread")
            .source_label("Notion")
            .when_ts(
                first
                    .get("created_time")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
            .author(short_author(first_author_id, user_names))
            .conversation_name(Some(page_title.to_string()))
            .conversation_uuid(disc_id)
            .entire_chat(format!("/notion/thread/{disc_id}"))
            .text(aggregated_text)
            .qmd_path(Some(thread_qmd.clone()))
            .source_url(Some(thread_url.clone()))
            .notion_page_uuid(Some(page_id.to_string()))
            .notion_block_uuid(parent_block_id.map(String::from))
            .markdown_uuid(Some(disc_id.to_string()))
            .build()?,
    );
    for (idx, c) in members_sorted.iter().enumerate() {
        let author_id = c
            .get("created_by")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        rows.push(
            GridRow::builder()
                .uuid(c.get("id").and_then(|v| v.as_str()).unwrap_or(""))
                .provider("notion")
                .kind("Notion Comment")
                .source_label("Notion")
                .when_ts(
                    c.get("created_time")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                )
                .author(short_author(author_id, user_names))
                .conversation_name(Some(page_title.to_string()))
                .conversation_uuid(disc_id)
                .message_index(Some(idx as i64))
                .entire_chat(format!("/notion/thread/{disc_id}"))
                .text(comment_text_plain(c))
                .qmd_path(Some(thread_qmd.clone()))
                .source_url(Some(thread_url.clone()))
                .notion_page_uuid(Some(page_id.to_string()))
                .notion_block_uuid(parent_block_id.map(String::from))
                .markdown_uuid(Some(disc_id.to_string()))
                .build()?,
        );
    }
    Ok(rows)
}

fn canonical_json(v: &Value) -> String {
    serde_json::to_string(&canonicalize(v)).unwrap_or_default()
}

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(pairs.len());
            for (k, val) in pairs {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn fingerprint_for_page(
    page: &Value,
    blocks_sorted: &[&Value],
    comments_sorted: &[&Value],
) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    canonical_json(page).hash(&mut h);
    for b in blocks_sorted {
        canonical_json(b).hash(&mut h);
    }
    for c in comments_sorted {
        canonical_json(c).hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn fingerprint_for_discussion(comments_sorted: &[&Value]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    for c in comments_sorted {
        canonical_json(c).hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Result of gathering per-document row sets from a parsed Notion tree.
pub struct DocumentRows {
    /// One per page.
    pub pages: Vec<PageDocument>,
    /// One per discussion.
    pub threads: Vec<ThreadDocument>,
}

pub struct PageDocument {
    pub page_uuid: String,
    pub page_title: String,
    pub rows: Vec<GridRow>,
    pub source_fingerprint: String,
}

pub struct ThreadDocument {
    pub discussion_uuid: String,
    pub page_uuid: String,
    pub page_title: String,
    pub rows: Vec<GridRow>,
    pub source_fingerprint: String,
}

pub fn gather_documents(parsed: &ParsedNotionOfficial) -> Result<DocumentRows> {
    // Phase-by-phase timing for `notion_unittests`'s
    // `gather_documents_is_linear_in_blocks` regression test (and any
    // real sync where this function appears in a flame graph). Each
    // phase emits a single `tracing::debug!` event with the elapsed
    // wall time. Production sync runs with the default INFO filter
    // see nothing; the unit test enables a stderr subscriber so the
    // breakdown shows up alongside the assertion.
    let total_start = Instant::now();
    let phase_start = Instant::now();

    let page_titles = build_page_titles(&parsed.pages, &parsed.blocks);
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "build_page_titles",
        pages = parsed.pages.len(),
        blocks = parsed.blocks.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    let mut pages: Vec<PageDocument> = Vec::new();

    // Group blocks by their owning page (for fingerprint stability — we
    // include every block whose tree roots at this page).
    let phase_start = Instant::now();
    let block_owning_page = block_to_page_id(&parsed.blocks);
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "block_to_page_id",
        blocks = parsed.blocks.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    // Build the block→parent-block index once; the previous version
    // rebuilt this map inside the outer loop, making the whole pass
    // O(N²) over `parsed.blocks` and pegging a core on real notion
    // sources (~10K blocks).
    let phase_start = Instant::now();
    let mut block_parent: HashMap<String, String> = HashMap::new();
    for bb in &parsed.blocks {
        let par = bb.get("parent");
        if par.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("block_id") {
            let id = bb
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pp = par
                .and_then(|v| v.get("block_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() && !pp.is_empty() {
                block_parent.insert(id, pp);
            }
        }
    }
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "build_block_parent",
        blocks = parsed.blocks.len(),
        parent_edges = block_parent.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    // Owner-walk: for each block, follow parent pointers up to the
    // page. Memoized: as soon as we know any block's owner, we record
    // it; subsequent walks short-circuit when they hit a memoized
    // ancestor. Without the memo the pass is O(N · avg_depth) — on the
    // regression-test fixture (a 4000-deep linear parent chain) that's
    // ~8M HashMap lookups (6s in debug). With the memo it's amortized
    // O(N) regardless of tree shape; the same fixture finishes in
    // <50ms. Real notion trees are depth ~10 so the memo doesn't help
    // production much in walltime, but it removes a quadratic cliff
    // that was easy to fall off (a 10K-block source with a
    // pathological subtree could repeat the regression).
    let phase_start = Instant::now();
    let mut blocks_by_page: HashMap<String, Vec<&Value>> = HashMap::new();
    let mut block_owner_memo: HashMap<String, String> = HashMap::new();
    let max_walk = parsed.blocks.len() + 1; // cycle safeguard
    for b in &parsed.blocks {
        let bid = b
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if bid.is_empty() {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut cur: Option<String> = Some(bid);
        let mut owner: Option<String> = None;
        while let Some(c) = cur {
            // Check the direct block→page index, then the memoized
            // resolution. Either path terminates the walk.
            if let Some(p) = block_owning_page.get(&c) {
                owner = Some(p.clone());
                break;
            }
            if let Some(p) = block_owner_memo.get(&c) {
                owner = Some(p.clone());
                break;
            }
            path.push(c.clone());
            if path.len() > max_walk {
                // Cycle in the parent graph — give up rather than
                // loop forever. Cleaner than the old HashSet-per-walk
                // and just as safe given the bounded blocks count.
                break;
            }
            cur = block_parent.get(&c).cloned();
        }
        if let Some(o) = owner {
            // Memoize every block we walked through so any future
            // block whose chain passes through here terminates in O(1).
            for p in path {
                block_owner_memo.insert(p, o.clone());
            }
            blocks_by_page.entry(o).or_default().push(b);
        }
    }
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "walk_blocks_to_owner",
        blocks = parsed.blocks.len(),
        pages_with_blocks = blocks_by_page.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    let phase_start = Instant::now();
    for vec in blocks_by_page.values_mut() {
        vec.sort_by(|a, b| {
            let ai = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let bi = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ai.cmp(bi)
        });
    }

    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "sort_blocks_per_page",
        pages_with_blocks = blocks_by_page.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    // Group comments by owning page (for fingerprint stability).
    let phase_start = Instant::now();
    let mut comments_by_page: HashMap<String, Vec<&Value>> = HashMap::new();
    for c in &parsed.comments {
        if let Some(pid) = resolve_comment_page_id(c, &parsed.blocks, &block_owning_page) {
            comments_by_page.entry(pid).or_default().push(c);
        }
    }
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "comments_by_page",
        comments = parsed.comments.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );
    let phase_start = Instant::now();
    for vec in comments_by_page.values_mut() {
        vec.sort_by(|a, b| {
            let ai = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let bi = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ai.cmp(bi)
        });
    }

    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "sort_comments_per_page",
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    let phase_start = Instant::now();
    for page in &parsed.pages {
        let pid = page
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if pid.is_empty() {
            continue;
        }
        let title = page_titles
            .get(&pid)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(untitled)".into());
        let row = page_row(page, &title, &parsed.user_names)?;
        let empty: Vec<&Value> = Vec::new();
        let blocks = blocks_by_page.get(&pid).unwrap_or(&empty);
        let comments = comments_by_page.get(&pid).unwrap_or(&empty);
        let fp = fingerprint_for_page(page, blocks, comments);
        pages.push(PageDocument {
            page_uuid: pid,
            page_title: title,
            rows: vec![row],
            source_fingerprint: fp,
        });
    }
    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "page_documents",
        pages = pages.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    // Discussions.
    let phase_start = Instant::now();
    let mut by_disc: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for c in &parsed.comments {
        let did = c
            .get("discussion_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if did.is_empty() {
            continue;
        }
        by_disc.entry(did.into()).or_default().push(c.clone());
    }
    let mut threads: Vec<ThreadDocument> = Vec::new();
    for (disc_id, mut members) in by_disc {
        members.sort_by(|a, b| {
            let aa = a.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
            let bb = b.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
            let ai = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let bi = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            aa.cmp(bb).then(ai.cmp(bi))
        });
        let first = &members[0];
        let Some(page_id) = resolve_comment_page_id(first, &parsed.blocks, &block_owning_page)
        else {
            continue;
        };
        let page_title = page_titles
            .get(&page_id)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(untitled)".into());
        let parent = first.get("parent").cloned().unwrap_or(Value::Null);
        let parent_block_id = if parent.get("type").and_then(|v| v.as_str()) == Some("block_id") {
            parent
                .get("block_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        } else {
            None
        };
        let rows = thread_rows(
            &disc_id,
            &members,
            &page_id,
            &page_title,
            parent_block_id.as_deref(),
            &parsed.user_names,
        )?;
        // Fingerprint over sorted-by-id comments for stability.
        let mut for_fp: Vec<&Value> = members.iter().collect();
        for_fp.sort_by(|a, b| {
            let ai = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let bi = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ai.cmp(bi)
        });
        let fp = fingerprint_for_discussion(&for_fp);
        threads.push(ThreadDocument {
            discussion_uuid: disc_id,
            page_uuid: page_id,
            page_title,
            rows,
            source_fingerprint: fp,
        });
    }

    tracing::debug!(
        event = "notion_gather_documents_phase",
        phase = "thread_documents",
        threads = threads.len(),
        elapsed_ms = phase_start.elapsed().as_millis() as u64,
    );

    tracing::debug!(
        event = "notion_gather_documents_total",
        pages_in = parsed.pages.len(),
        blocks_in = parsed.blocks.len(),
        comments_in = parsed.comments.len(),
        pages_out = pages.len(),
        threads_out = threads.len(),
        elapsed_ms = total_start.elapsed().as_millis() as u64,
    );

    Ok(DocumentRows { pages, threads })
}

// Silence unused-import warning for slugify (re-exported from render
// for the sidecar path; some downstream tests poke at it).
#[allow(dead_code)]
fn _slugify_smoke(s: &str) -> String {
    slugify(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Once;
    use std::time::Duration;

    // Diagnostic output: when this test ever blows its time budget (or
    // anyone wonders why `notion_unittests` takes >5s wall), running
    // with `--test-output=streamed` shows the per-phase breakdown
    // emitted by `gather_documents`. The slowest test in this file
    // dominates the binary's wall time — see the comment block on
    // `gather_documents_is_linear_in_blocks` below.
    static INIT_TRACING: Once = Once::new();
    fn init_tracing() {
        INIT_TRACING.call_once(|| {
            // Best-effort — if a global subscriber is already set, do
            // nothing (avoids the "panic" on double-set when an
            // outer test harness already initialized one).
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(std::io::stderr)
                .try_init();
        });
    }

    // Regression test for a quadratic blow-up in `gather_documents`.
    // History:
    //
    //   v1 bug: rebuilt the `block_parent` map (an inner O(N) scan
    //   over `parsed.blocks`) inside the outer `for b in
    //   &parsed.blocks` loop. Real ~10K-block notion sources pegged
    //   a core indefinitely.
    //
    //   v2 fix: build `block_parent` once outside the loop. But the
    //   owner-walk itself was still O(depth) per block; on the
    //   pathological linear-chain test fixture below (N=4_000, each
    //   block parents the previous one) the walk did ~8M HashMap
    //   lookups and took 6s in debug. Real notion trees are depth
    //   ~10 so production was fine, but the test budget was 30s and
    //   the test binary's wall was dominated by this one test.
    //
    //   v3 (current): memoize `block → owning_page` during the walk.
    //   Each block resolves in O(1) amortized regardless of tree
    //   shape; the same N=4_000 fixture finishes in ~50ms.
    //
    // Budget below stays generous (1s) so we catch a regression to
    // the v1/v2 cliff long before someone sees production wedge,
    // without flaking on CI under load. If anything in this file
    // pushes wall above ~200ms the per-phase
    // `notion_gather_documents_phase` debug events surface where —
    // run with `--test_output=streamed --cache_test_results=no`.
    #[test]
    fn gather_documents_is_linear_in_blocks() {
        init_tracing();
        const N: usize = 4_000;
        let page_id = "page-1";
        let pages = vec![json!({"id": page_id, "object": "page"})];
        let mut blocks = Vec::with_capacity(N);
        // First block parents directly to the page; each subsequent
        // block parents to the previous block. With the quadratic bug,
        // the inner loop builds an N-entry HashMap N times.
        blocks.push(json!({
            "id": "block-000000",
            "object": "block",
            "type": "paragraph",
            "parent": {"type": "page_id", "page_id": page_id},
            "page_id": page_id,
        }));
        for i in 1..N {
            blocks.push(json!({
                "id": format!("block-{i:06}"),
                "object": "block",
                "type": "paragraph",
                "parent": {"type": "block_id", "block_id": format!("block-{:06}", i - 1)},
                "page_id": page_id,
            }));
        }
        let parsed = ParsedNotionOfficial {
            pages,
            blocks,
            ..ParsedNotionOfficial::default()
        };

        let start = Instant::now();
        let docs = gather_documents(&parsed).expect("valid notion grid rows");
        let elapsed = start.elapsed();
        // Surface the wall time on stderr so `bazel test
        // --test_output=streamed` shows it next to the
        // `notion_gather_documents_phase` events from inside the
        // function — at a glance you can tell which phase grew when
        // the binary's total wall time creeps up.
        tracing::info!(
            event = "gather_documents_is_linear_in_blocks",
            n = N,
            elapsed_ms = elapsed.as_millis() as u64,
            "smoke regression for the quadratic block_parent rebuild",
        );

        assert_eq!(docs.pages.len(), 1);
        // 1s budget: a healthy run finishes in ~50ms (memoized walk),
        // so 1s gives ~20× headroom for CI core contention while still
        // catching a regression to the v1/v2 cliffs (6s or
        // never-finishes) long before they reach production.
        assert!(
            elapsed < Duration::from_secs(1),
            "gather_documents took {elapsed:?} for {N} blocks — likely a regression \
             in the memoized owner-walk; run `bazel test … --test_output=streamed \
             --cache_test_results=no` and look at notion_gather_documents_phase events",
        );
    }
}
