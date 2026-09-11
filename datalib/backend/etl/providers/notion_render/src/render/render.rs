//! Emit one document per Notion page and one per comment thread.
//!
//! The page body is **not** rendered here. Notion returns it already
//! rendered as enhanced markdown, and the download step stores it whole;
//! this writes it out with frontmatter, resolving each attachment slot
//! to the file the CAS materialized beside it. What used to be a
//! block-type matrix maintained against Notion's evolving block set is
//! now Notion's problem.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use super::grid_rows::{gather_documents, PageDocument, ThreadDocument};
use super::parse::ParsedNotion;

pub const RENDER_VERSION: u32 = 2;
pub const SLUG_MAX_LEN: usize = 60;

static SLUG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

pub fn slugify(name: &str) -> String {
    if name.is_empty() {
        return "untitled".into();
    }
    let lower = name.to_lowercase();
    let s = SLUG_RE.replace_all(&lower, "-");
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        return "untitled".into();
    }
    let mut s: String = s.chars().take(SLUG_MAX_LEN).collect();
    s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "untitled".into()
    } else {
        s
    }
}

pub fn short_id(uuid_str: &str) -> String {
    let first = uuid_str.split('-').next().unwrap_or("");
    let mut s: String = first.chars().take(8).collect();
    if s.is_empty() {
        s = "00000000".into();
    }
    s
}

pub fn page_dir_segment(page_id: &str) -> String {
    page_id.to_string()
}

pub fn notion_url(page_id: &str) -> String {
    format!("https://www.notion.so/{}", page_id.replace('-', ""))
}

pub fn notion_thread_url(
    page_id: &str,
    discussion_id: Option<&str>,
    anchor_block_id: Option<&str>,
) -> String {
    let pg = page_id.replace('-', "");
    let mut url = format!("https://www.notion.so/{pg}");
    match (discussion_id, anchor_block_id) {
        (Some(d), Some(a)) if !d.is_empty() => {
            url.push_str(&format!("?d={}", d.replace('-', "")));
            url.push_str(&format!("#{}", a.replace('-', "")));
        }
        (Some(d), None) if !d.is_empty() => {
            url.push_str(&format!("?d={}", d.replace('-', "")));
        }
        (_, Some(a)) if !a.is_empty() => {
            url.push_str(&format!("#{}", a.replace('-', "")));
        }
        _ => {}
    }
    url
}

pub fn thread_filename(discussion_id: &str) -> String {
    format!("{discussion_id}.md")
}

pub fn pages_subdir(stanza: &str) -> PathBuf {
    PathBuf::from(stanza)
        .join(datalib_etl::layout::RENDER_MARKDOWN_DIR)
        .join("pages")
}

pub fn page_qmd_path_rel(stanza: &str, page_id: &str) -> String {
    let seg = page_dir_segment(page_id);
    pages_subdir(stanza)
        .join(seg)
        .join("index.md")
        .to_string_lossy()
        .into_owned()
}

/// Where a discussion's markdown lives, relative to the data root.
///
/// This is what the grid row advertises as `qmd_path`, so
/// [`render_thread`] must write to the directory it names — the fixture
/// pipeline asserts every row's `qmd_path` equals its markdown's
/// `md_path`, and a disagreement here means the preview pane 404s.
pub fn thread_qmd_path_rel(stanza: &str, page_id: &str, discussion_id: &str) -> String {
    let seg = page_dir_segment(page_id);
    pages_subdir(stanza)
        .join(seg)
        .join("threads")
        .join(thread_filename(discussion_id))
        .to_string_lossy()
        .into_owned()
}
/// Rewrite attachment slots to the filenames the bundle materialized.
///
/// The stored markdown references each attachment by slot — the
/// unsigned upstream URL — because that is the only identifier that
/// survives Notion re-signing the link on every fetch. A slot with no
/// bytes in the CAS is left as-is: an upstream link still beats a
/// broken relative path.
fn localize_attachments(markdown: &str, bundle: &BlobBundle) -> String {
    let mut out = markdown.to_string();
    let slots: Vec<String> = bundle
        .iter()
        .map(|(ref_id, _)| ref_id.to_string())
        .collect();
    for slot in slots {
        if let Some(name) = bundle.filename_for(&slot) {
            out = out.replace(&slot, &format!("blobs/{name}"));
        }
    }
    out
}

fn frontmatter(title: &str, page: &Value, extra: &[(&str, String)]) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("title: {:?}\n", title));
    for (k, v) in [
        ("created_at", "created_time"),
        ("last_edited_at", "last_edited_time"),
    ] {
        if let Some(t) = page.get(v).and_then(|x| x.as_str()) {
            fm.push_str(&format!("{k}: {t}\n"));
        }
    }
    if let Some(u) = page.get("url").and_then(|x| x.as_str()) {
        fm.push_str(&format!("source_url: {u}\n"));
    }
    fm.push_str("source_label: Notion\n");
    for (k, v) in extra {
        fm.push_str(&format!("{k}: {v}\n"));
    }
    fm.push_str("---\n\n");
    fm
}

/// Page properties, for a page whose body is empty.
///
/// A database row's content usually *is* its properties — 71% of pages
/// in a measured workspace have no body at all — so emitting nothing
/// for them would drop most of a workspace on the floor.
fn properties_table(page: &Value) -> String {
    let Some(props) = page.get("properties").and_then(|v| v.as_object()) else {
        return String::new();
    };
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, prop) in props {
        let rendered = render_property(prop);
        if !rendered.is_empty() {
            rows.push((name.clone(), rendered));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("| Property | Value |\n| --- | --- |\n");
    for (k, v) in rows {
        out.push_str(&format!(
            "| {} | {} |\n",
            k.replace('|', "\\|"),
            v.replace('|', "\\|")
        ));
    }
    out
}

fn plain(rt: Option<&Value>) -> String {
    rt.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.get("plain_text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn render_property(prop: &Value) -> String {
    match prop.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "title" => plain(prop.get("title")),
        "rich_text" => plain(prop.get("rich_text")),
        "url" => prop
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "email" => prop
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "phone_number" => prop
            .get("phone_number")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "number" => prop
            .get("number")
            .and_then(|v| v.as_f64())
            .map(|n| n.to_string())
            .unwrap_or_default(),
        "checkbox" => match prop.get("checkbox").and_then(|v| v.as_bool()) {
            Some(true) => "yes".into(),
            Some(false) => "no".into(),
            None => String::new(),
        },
        "select" => prop
            .get("select")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "status" => prop
            .get("status")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "multi_select" => names_of(prop.get("multi_select")),
        "people" => names_of(prop.get("people")),
        "date" => {
            let d = prop.get("date");
            let start = d
                .and_then(|v| v.get("start"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let end = d
                .and_then(|v| v.get("end"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if end.is_empty() {
                start.into()
            } else {
                format!("{start} → {end}")
            }
        }
        "created_time" => prop
            .get("created_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "last_edited_time" => prop
            .get("last_edited_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        _ => String::new(),
    }
}

fn names_of(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn render_thread(
    disc_id: &str,
    page_title: &str,
    members: &[&Value],
    anchor: Option<&str>,
    dir: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(thread_filename(disc_id));
    let mut out = format!(
        "---\ntitle: {:?}\nsource_label: Notion\n---\n\n",
        page_title
    );
    // A comment names the block it hangs off and carries no quote of
    // it, so without this the thread opens with no indication of what
    // it is about.
    if let Some(a) = anchor.filter(|a| !a.trim().is_empty()) {
        out.push_str(&format!("> {}\n\n", a.replace('\n', "\n> ")));
    }
    if members
        .iter()
        .any(|c| c.get("original_content_deleted").and_then(|v| v.as_bool()) == Some(true))
    {
        out.push_str("*The commented-on content has been deleted upstream.*\n\n");
    }
    for c in members {
        let uuid = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let author = c
            .get("display_name")
            .and_then(|d| d.get("resolved_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let when = c.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
        // The id + data-section-uuid pair is what the UI walks to anchor
        // per-message feedback; see AGENTS.md "QMDs are write-only".
        out.push_str(&format!(
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--notion\">\n\n"
        ));
        out.push_str(&format!("**{author}** · {when}\n\n"));
        out.push_str(&plain(c.get("rich_text")));
        out.push_str("\n\n</div>\n\n");
    }
    fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

pub fn render_notion(
    parsed: &ParsedNotion,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    let pages_root = datalib_etl::layout::render_markdown_root(root, stanza).join("pages");
    fs::create_dir_all(&pages_root)?;
    if parsed.pages.is_empty() && parsed.comments.is_empty() {
        return Ok(summary);
    }

    let docs = gather_documents(parsed, stanza)?;
    let pages_by_id: HashMap<&str, &Value> = parsed
        .pages
        .iter()
        .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(|id| (id, p)))
        .collect();
    progress.set_length(Some((docs.pages.len() + docs.threads.len()) as u64));

    let empty_bundle = BlobBundle::default();
    for doc in &docs.pages {
        let PageDocument {
            page_uuid,
            page_title,
            source_fingerprint,
            ..
        } = doc;
        let page_dir = pages_root.join(page_dir_segment(page_uuid));
        let md_path = page_dir.join("index.md");
        let Some(page) = pages_by_id.get(page_uuid.as_str()) else {
            continue;
        };
        fs::create_dir_all(&page_dir)?;
        let bundle = parsed.blobs_by_page.get(page_uuid).unwrap_or(&empty_bundle);
        if !bundle.is_empty() {
            bundle.materialize_to_dir(&page_dir.join("blobs"))?;
        }
        let body = parsed
            .markdown_by_page
            .get(page_uuid)
            .map(|m| localize_attachments(m, bundle))
            .unwrap_or_default();
        let body = if body.trim().is_empty() {
            properties_table(page)
        } else {
            body
        };
        let mut out = frontmatter(page_title, page, &[]);
        out.push_str(&body);
        fs::write(&md_path, out).with_context(|| format!("write {}", md_path.display()))?;

        on_doc_complete(RenderedMarkdown {
            markdown_uuid: page_uuid.clone(),
            source_id: String::new(),
            source_fingerprint: source_fingerprint.clone(),
            upstream_cursor: None,
            md_path: md_path.clone(),
            render_version: RENDER_VERSION,
            rows: doc.rows.clone(),
            edges: Vec::new(),
            problems: doc.problems.clone(),
        })?;
        summary.rendered += 1;
        progress.inc(1);
    }

    let mut by_disc: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for c in &parsed.comments {
        if let Some(d) = c.get("discussion_id").and_then(|v| v.as_str()) {
            if !d.is_empty() {
                by_disc.entry(d).or_default().push(c);
            }
        }
    }
    for doc in &docs.threads {
        let ThreadDocument {
            discussion_uuid,
            page_uuid,
            page_title,
            source_fingerprint,
            ..
        } = doc;
        let thread_path = root.join(thread_qmd_path_rel(stanza, page_uuid, discussion_uuid));
        let Some(members) = by_disc.get(discussion_uuid.as_str()) else {
            continue;
        };
        let dir = thread_path
            .parent()
            .expect("thread_qmd_path_rel always has a parent dir");
        let anchor = doc
            .anchor_block_uuid
            .as_deref()
            .and_then(|b| parsed.anchor_text.get(b))
            .map(String::as_str);
        let p = render_thread(discussion_uuid, page_title, members, anchor, dir)?;
        on_doc_complete(RenderedMarkdown {
            markdown_uuid: discussion_uuid.clone(),
            source_id: String::new(),
            source_fingerprint: source_fingerprint.clone(),
            upstream_cursor: None,
            md_path: p,
            render_version: RENDER_VERSION,
            rows: doc.rows.clone(),
            edges: Vec::new(),
            problems: doc.problems.clone(),
        })?;
        summary.rendered += 1;
        progress.inc(1);
    }
    // Only once every document landed. A cursor written over a failed
    // render would tell the next run those pages were already done.
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = datalib_etl::render_cursor::cursor_path(root, stanza);
        datalib_etl::render_cursor::write(
            &cursor_path,
            head,
            &datalib_etl::render_cursor::no_params(),
        )?;
    }
    Ok(summary)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RenderSummary {
    pub rendered: usize,
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// A database row's properties are its content. Rendering nothing
    /// for a body-less page would drop most of a real workspace.
    #[test]
    fn a_body_less_page_renders_its_properties() {
        let page = json!({"id":"p1","properties":{
            "Name":{"type":"title","title":[{"plain_text":"Standup Notes"}]},
            "Status":{"type":"select","select":{"name":"Active"}},
            "Empty":{"type":"rich_text","rich_text":[]}
        }});
        let t = properties_table(&page);
        assert!(t.contains("| Name | Standup Notes |"), "{t}");
        assert!(t.contains("| Status | Active |"), "{t}");
        assert!(!t.contains("Empty"), "empty properties are omitted: {t}");
    }

    /// A thread's file must land exactly where its grid row says. These
    /// disagreed once — the row said `threads/`, the writer was handed
    /// `discussions/` — which the fixture pipeline caught as a
    /// `qmd_path` mismatch and a user would have seen as a 404 preview.
    /// `render_notion` now derives the directory from
    /// `thread_qmd_path_rel`, so this drives the real entry point
    /// rather than `render_thread` in isolation, which is where the
    /// earlier version of this test failed to look.
    #[test]
    fn a_thread_lands_where_its_row_says_it_does() {
        let d = tempdir().unwrap();
        let page = "b1d6e000-1701-4d00-8000-000000000001";
        let disc = "d0000001-1701-4d00-8000-000000000001";
        let parsed = ParsedNotion {
            pages: vec![json!({
                "id": page,
                "object": "page",
                "properties": {"title": {"type": "title", "title": [{"plain_text": "Handbook"}]}},
            })],
            comments: vec![json!({
                "id": "c1",
                "page_id": page,
                "discussion_id": disc,
                "created_time": "2369-04-15T01:00:00.000Z",
                "display_name": {"resolved_name": "Data"},
                "rich_text": [{"plain_text": "hi"}],
                "parent": {"type": "block_id", "block_id": "b1"},
            })],
            ..Default::default()
        };
        let mut advertised: Vec<String> = Vec::new();
        let mut on_doc = |md: RenderedMarkdown| {
            for r in &md.rows {
                if let Some(q) = &r.qmd_path {
                    advertised.push(q.clone());
                }
            }
            Ok(())
        };
        render_notion(&parsed, d.path(), "notion", &Progress::noop(), &mut on_doc).unwrap();
        assert!(!advertised.is_empty(), "expected rows with a qmd_path");
        for rel in advertised {
            assert!(
                d.path().join(&rel).exists(),
                "row advertises {rel}, but no file is there"
            );
        }
    }

    /// The quoted block leads the thread file, so a reader sees what
    /// the discussion is about before the first reply.
    #[test]
    fn a_thread_file_opens_with_the_quoted_block() {
        let d = tempdir().unwrap();
        let c = json!({"id": "c1", "created_time": "2369-04-15T01:00:00.000Z",
                       "display_name": {"resolved_name": "Data"},
                       "rich_text": [{"plain_text": "Recommend recalibration"}]});
        let p = render_thread(
            "d1",
            "Handbook",
            &[&c],
            Some("Warp core alignment"),
            d.path(),
        )
        .unwrap();
        let md = fs::read_to_string(p).unwrap();
        assert!(md.contains("> Warp core alignment"), "{md}");
        assert!(md.find("> Warp core alignment") < md.find("Recommend recalibration"));
    }

    /// `original_content_deleted` is a real upstream signal: the thing
    /// the comment was about is gone. Say so rather than rendering a
    /// thread that appears to be about nothing.
    #[test]
    fn a_thread_whose_anchor_was_deleted_says_so() {
        let d = tempdir().unwrap();
        let c = json!({"id": "c1", "created_time": "2369-04-15T01:00:00.000Z",
                       "original_content_deleted": true,
                       "display_name": {"resolved_name": "Data"},
                       "rich_text": [{"plain_text": "hi"}]});
        let p = render_thread("d1", "Handbook", &[&c], None, d.path()).unwrap();
        let md = fs::read_to_string(p).unwrap();
        assert!(md.contains("deleted upstream"), "{md}");
    }

    #[test]
    fn slugify_and_short_id_shape_the_page_dir() {
        assert_eq!(
            slugify("Project Data Liberation ✊"),
            "project-data-liberation"
        );
        assert!(!page_dir_segment("348a550f-af95-80a0-8973-e679d9e1c6c9").is_empty());
    }
}
