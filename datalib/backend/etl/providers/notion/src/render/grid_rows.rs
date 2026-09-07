//! `grid_rows` for the notion provider: one row per page, plus one
//! thread row and one comment row per discussion.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::time::Instant;

use anyhow::Result;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::render_problems::RenderProblemRow;
use serde_json::Value;

use super::parse::ParsedNotion;
use super::render::{notion_thread_url, notion_url, page_qmd_path_rel, thread_qmd_path_rel};

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

fn build_page_titles(pages: &[Value]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for p in pages {
        if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
            out.insert(id.to_string(), page_title_from(p));
        }
    }
    out
}

/// A comment's author name.
///
/// Notion resolves this for us: every comment carries
/// `display_name.resolved_name`. Falling back to a truncated user id
/// is what the previous implementation did for *every* comment,
/// because the name lookup it relied on was never populated.
fn comment_author(c: &Value) -> Option<String> {
    let name = c
        .get("display_name")
        .and_then(|d| d.get("resolved_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !name.is_empty() {
        return Some(name.to_string());
    }
    let uid = c
        .get("created_by")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    short_id_author(uid)
}

/// A page author. Unlike a comment, a page object carries only
/// `created_by.id`, so this needs the `users` table the download side
/// fills one id at a time.
fn resolved_author(uid: &str, user_names: &HashMap<String, String>) -> Option<String> {
    match user_names.get(uid) {
        Some(n) if !n.is_empty() => Some(n.clone()),
        _ => short_id_author(uid),
    }
}

/// Last resort when nothing named the user: the id's leading octet.
fn short_id_author(uid: &str) -> Option<String> {
    let s: String = uid.chars().take(8).collect();
    (!s.is_empty()).then_some(s)
}

fn page_row(
    page: &Value,
    title: &str,
    stanza: &str,
    user_names: &HashMap<String, String>,
    problems: &mut Vec<RenderProblemRow>,
) -> Option<GridRow> {
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
        .author(resolved_author(author_id, user_names))
        .conversation_name(Some(title.to_string()))
        .conversation_uuid(pid.clone())
        .entire_chat(format!("/notion/page/{pid}"))
        .text(title.to_string())
        .qmd_path(Some(page_qmd_path_rel(stanza, &pid)))
        .source_url(Some(notion_url(&pid)))
        .notion_page_uuid(Some(pid.clone()))
        .markdown_uuid(Some(pid.clone()))
        .build_or_record(stanza, &pid, RENDER_VERSION, problems)
}

#[allow(clippy::too_many_arguments)]
fn thread_rows(
    disc_id: &str,
    members_sorted: &[Value],
    page_id: &str,
    page_title: &str,
    stanza: &str,
    parent_block_id: Option<&str>,
    anchor: Option<&str>,
    problems: &mut Vec<RenderProblemRow>,
) -> Vec<GridRow> {
    if members_sorted.is_empty() {
        return Vec::new();
    }
    let thread_qmd = thread_qmd_path_rel(stanza, page_id, disc_id);
    let thread_url = notion_thread_url(page_id, Some(disc_id), parent_block_id);
    let mut rows: Vec<GridRow> = Vec::new();
    let first = &members_sorted[0];
    let mut aggregated_text: String = members_sorted
        .iter()
        .map(comment_text_plain)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // The quoted text leads the thread's searchable body: a comment
    // means little without what it is about, and Notion does not put
    // the two together anywhere.
    if let Some(a) = anchor.filter(|a| !a.is_empty()) {
        aggregated_text = format!("{a}\n{aggregated_text}");
    }
    rows.extend(
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
            .author(comment_author(first))
            .conversation_name(Some(page_title.to_string()))
            .conversation_uuid(disc_id)
            .entire_chat(format!("/notion/thread/{disc_id}"))
            .text(aggregated_text)
            .qmd_path(Some(thread_qmd.clone()))
            .source_url(Some(thread_url.clone()))
            .notion_page_uuid(Some(page_id.to_string()))
            .notion_block_uuid(parent_block_id.map(String::from))
            .markdown_uuid(Some(disc_id.to_string()))
            .build_or_record(stanza, disc_id, RENDER_VERSION, problems),
    );
    for (idx, c) in members_sorted.iter().enumerate() {
        rows.extend(
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
                .author(comment_author(c))
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
                .build_or_record(stanza, disc_id, RENDER_VERSION, problems),
        );
    }
    rows
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

/// A page's fingerprint: its own payload plus its body plus its
/// comments. The body is included directly rather than via its blocks,
/// which is only sound because the stored markdown is stable for an
/// unchanged page — signed attachment URLs are reduced to slots before
/// storage (`download::slots`). Were they left signed, every page with
/// an image would re-render on every run.
fn fingerprint_for_page(page: &Value, markdown: &str, comments: &[&Value]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical_json(page).hash(&mut h);
    markdown.hash(&mut h);
    for c in comments {
        canonical_json(c).hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn fingerprint_for_discussion(comments_sorted: &[&Value]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
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
    /// What this document lost on the way here; travels with the rows
    /// so both commit together.
    pub problems: Vec<RenderProblemRow>,
}

pub struct ThreadDocument {
    pub discussion_uuid: String,
    pub page_uuid: String,
    pub page_title: String,
    /// The block this thread hangs off, when it hangs off one. Render
    /// resolves it to the quoted text via `ParsedNotion::anchor_text`.
    pub anchor_block_uuid: Option<String>,
    pub rows: Vec<GridRow>,
    pub source_fingerprint: String,
    /// See [`PageDocument::problems`].
    pub problems: Vec<RenderProblemRow>,
}

pub fn gather_documents(parsed: &ParsedNotion, stanza: &str) -> Result<DocumentRows> {
    let t0 = Instant::now();
    let page_titles = build_page_titles(&parsed.pages);

    // Comments carry their owning page id from the download side, so
    // there is nothing to resolve here. Mapping a comment back to its
    // page used to need a walk up the block tree — that is why this
    // function once had to be checked for linearity in the block count.
    let mut comments_by_page: HashMap<String, Vec<&Value>> = HashMap::new();
    let mut comments_by_discussion: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for c in &parsed.comments {
        if let Some(pid) = c.get("page_id").and_then(|v| v.as_str()) {
            comments_by_page.entry(pid.to_string()).or_default().push(c);
        }
        if let Some(did) = c.get("discussion_id").and_then(|v| v.as_str()) {
            if !did.is_empty() {
                comments_by_discussion
                    .entry(did.to_string())
                    .or_default()
                    .push(c);
            }
        }
    }
    let by_created = |a: &&Value, b: &&Value| {
        let k = |v: &Value| {
            v.get("created_time")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        k(a).cmp(&k(b))
    };
    for v in comments_by_page.values_mut() {
        v.sort_by(by_created);
    }
    for v in comments_by_discussion.values_mut() {
        v.sort_by(by_created);
    }

    // ── page documents ───────────────────────────────────────────────
    let mut pages: Vec<PageDocument> = Vec::with_capacity(parsed.pages.len());
    let empty: Vec<&Value> = Vec::new();
    for page in &parsed.pages {
        let Some(pid) = page.get("id").and_then(|v| v.as_str()).map(String::from) else {
            continue;
        };
        let title = page_titles.get(&pid).cloned().unwrap_or_default();
        let markdown = parsed
            .markdown_by_page
            .get(&pid)
            .map(String::as_str)
            .unwrap_or("");
        let comments = comments_by_page.get(&pid).unwrap_or(&empty);
        let mut problems: Vec<RenderProblemRow> = Vec::new();
        let mut rows: Vec<GridRow> = Vec::new();
        if let Some(r) = page_row(page, &title, stanza, &parsed.user_names, &mut problems) {
            rows.push(r);
        }
        pages.push(PageDocument {
            source_fingerprint: fingerprint_for_page(page, markdown, comments),
            page_uuid: pid,
            page_title: title,
            rows,
            problems,
        });
    }

    // ── thread documents ─────────────────────────────────────────────
    let mut threads: Vec<ThreadDocument> = Vec::new();
    for (disc_id, members) in &comments_by_discussion {
        let first = members[0];
        let Some(page_id) = first
            .get("page_id")
            .and_then(|v| v.as_str())
            .map(String::from)
        else {
            continue;
        };
        let title = page_titles
            .get(&page_id)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(untitled)".into());
        let parent_block_id = first
            .get("parent")
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("block_id"))
            .and_then(|p| p.get("block_id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let owned: Vec<Value> = members.iter().map(|v| (*v).clone()).collect();
        let mut problems: Vec<RenderProblemRow> = Vec::new();
        let anchor = parent_block_id
            .as_deref()
            .and_then(|b| parsed.anchor_text.get(b))
            .map(String::as_str);
        let rows = thread_rows(
            disc_id,
            &owned,
            &page_id,
            &title,
            stanza,
            parent_block_id.as_deref(),
            anchor,
            &mut problems,
        );
        threads.push(ThreadDocument {
            source_fingerprint: fingerprint_for_discussion(members),
            discussion_uuid: disc_id.clone(),
            page_uuid: page_id,
            page_title: title,
            anchor_block_uuid: parent_block_id.clone(),
            rows,
            problems,
        });
    }

    tracing::debug!(
        event = "notion_gather_documents",
        pages = pages.len(),
        threads = threads.len(),
        elapsed_ms = t0.elapsed().as_millis() as u64,
    );
    Ok(DocumentRows { pages, threads })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(id: &str, title: &str) -> Value {
        json!({
            "id": id,
            "object": "page",
            "last_edited_time": "2026-01-01T00:00:00.000Z",
            "properties": {"title": {"type": "title", "title": [{"plain_text": title}]}},
        })
    }

    fn comment(id: &str, page_id: &str, disc: &str, who: &str, when: &str) -> Value {
        json!({
            "id": id,
            "object": "comment",
            "page_id": page_id,
            "discussion_id": disc,
            "created_time": when,
            "created_by": {"id": "47b71049-b1e7-4fc2-9d1e-4528dd803a62"},
            "display_name": {"type": "user", "resolved_name": who},
            "rich_text": [{"plain_text": "hello"}],
            "parent": {"type": "block_id", "block_id": "b-1"},
        })
    }

    /// A comment now carries its owning page id, so grouping needs no
    /// block walk. This is what let ~250 lines of block-tree traversal
    /// go, and with it the linearity regression test that guarded it.
    #[test]
    fn comments_group_into_threads_by_their_recorded_page() {
        let parsed = ParsedNotion {
            pages: vec![page("p1", "Standup Notes")],
            markdown_by_page: [("p1".to_string(), "# Notes\n".to_string())]
                .into_iter()
                .collect(),
            comments: vec![
                comment("c2", "p1", "d1", "Cathy Zhao", "2026-01-02T00:00:00.000Z"),
                comment("c1", "p1", "d1", "Cathy Zhao", "2026-01-01T00:00:00.000Z"),
            ],
            ..Default::default()
        };
        let docs = gather_documents(&parsed, "notion").unwrap();
        assert_eq!(docs.pages.len(), 1);
        assert_eq!(docs.threads.len(), 1);
        let t = &docs.threads[0];
        assert_eq!(t.page_uuid, "p1");
        assert_eq!(t.page_title, "Standup Notes");
        // thread row + one row per comment, oldest first
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[1].uuid, "c1");
        assert_eq!(t.rows[2].uuid, "c2");
    }

    /// A page object carries only `created_by.id`, so a page author
    /// needs the `users` table. Before that table existed every Notion
    /// page in the grid showed a truncated uuid as its author.
    #[test]
    fn page_authors_use_the_resolved_user_name() {
        let mut p = page("p1", "Handbook");
        p["created_by"] = json!({"object": "user", "id": "47b71049-b1e7-4fc2-9d1e-4528dd803a62"});
        let names: HashMap<String, String> = [(
            "47b71049-b1e7-4fc2-9d1e-4528dd803a62".to_string(),
            "Nayana Bannur".to_string(),
        )]
        .into_iter()
        .collect();
        let parsed = ParsedNotion {
            pages: vec![p],
            user_names: names,
            ..Default::default()
        };
        let docs = gather_documents(&parsed, "notion").unwrap();
        assert_eq!(
            docs.pages[0].rows[0].author.as_deref(),
            Some("Nayana Bannur")
        );
    }

    /// An unresolved id still has to render something, and must not
    /// block the page.
    #[test]
    fn an_unresolved_page_author_falls_back_to_an_id_prefix() {
        let mut p = page("p1", "Handbook");
        p["created_by"] = json!({"id": "47b71049-b1e7-4fc2-9d1e-4528dd803a62"});
        let parsed = ParsedNotion {
            pages: vec![p],
            ..Default::default()
        };
        let docs = gather_documents(&parsed, "notion").unwrap();
        assert_eq!(docs.pages[0].rows[0].author.as_deref(), Some("47b71049"));
    }

    /// A thread's searchable text has to include what the comment is
    /// about. Notion puts the comment and the commented-on block in
    /// different places and never joins them.
    #[test]
    fn a_thread_row_carries_its_anchor_text() {
        let parsed = ParsedNotion {
            pages: vec![page("p1", "Handbook")],
            comments: vec![comment(
                "c1",
                "p1",
                "d1",
                "Data",
                "2026-01-01T00:00:00.000Z",
            )],
            anchor_text: [("b-1".to_string(), "Warp core alignment".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let docs = gather_documents(&parsed, "notion").unwrap();
        let thread = &docs.threads[0];
        assert_eq!(thread.anchor_block_uuid.as_deref(), Some("b-1"));
        assert!(
            thread.rows[0].text.contains("Warp core alignment"),
            "thread text should lead with the anchor: {:?}",
            thread.rows[0].text
        );
    }

    /// Notion resolves comment authors for us via
    /// `display_name.resolved_name`. The previous implementation
    /// threaded a name map that was never populated, so every author
    /// rendered as a truncated uuid.
    #[test]
    fn comment_authors_use_the_name_notion_resolved() {
        let c = comment("c1", "p1", "d1", "Cathy Zhao", "2026-01-01T00:00:00.000Z");
        assert_eq!(comment_author(&c).as_deref(), Some("Cathy Zhao"));
        let mut anon = c.clone();
        anon["display_name"] = json!({"type": "user"});
        assert_eq!(comment_author(&anon).as_deref(), Some("47b71049"));
    }

    /// The fingerprint has to move when the body moves — that is what
    /// makes an edited page re-render — and stay put otherwise.
    #[test]
    fn the_page_fingerprint_tracks_the_body() {
        let p = page("p1", "A");
        let a = fingerprint_for_page(&p, "# one\n", &[]);
        let b = fingerprint_for_page(&p, "# one\n", &[]);
        let c = fingerprint_for_page(&p, "# two\n", &[]);
        assert_eq!(a, b, "same input must fingerprint identically");
        assert_ne!(a, c, "a changed body must change the fingerprint");
    }

    /// A database row has no body. It still needs a page document, or
    /// most of a real workspace never reaches the grid.
    #[test]
    fn a_body_less_page_still_produces_a_document() {
        let parsed = ParsedNotion {
            pages: vec![page("row1", "A row")],
            ..Default::default()
        };
        let docs = gather_documents(&parsed, "notion").unwrap();
        assert_eq!(docs.pages.len(), 1);
        assert_eq!(docs.pages[0].rows.len(), 1);
    }
}
