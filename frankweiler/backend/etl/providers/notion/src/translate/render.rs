//! Render Notion pages (mirrored by the official API) to markdown.
//!
//! Port of `src/ingest/render_notion_official.py`. Each page becomes
//! `<slug>__<id8>/index.md`; sub-pages render as sibling directories.
//! Comment threads land under `<page-dir>/threads/<disc-id8>__<slug>.md`,
//! deep-linked to the anchor block via `<a id="b-…">` markers emitted
//! around each block.
//!
//! When debugging "what is this block type supposed to render as?", a
//! useful cross-reference is the actively-maintained Node renderer at
//! <https://github.com/souvikinator/notion-to-md>. We don't shell out
//! to it (we need our own QMD-with-section-divs shape + grid_rows
//! sidecar, both of which it doesn't produce), but its block handlers
//! are a good "is our output reasonable?" oracle.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::BlobReader;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};
use frankweiler_etl::title::Title;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use super::grid_rows::{gather_documents, PageDocument, ThreadDocument};
use super::parse::ParsedNotionOfficial;

pub const RENDER_VERSION: u32 = 1;
pub const SLUG_MAX_LEN: usize = 60;

static SLUG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub rendered: usize,
    pub orphans_removed: usize,
    pub skipped: usize,
}

// ----- slug / path helpers -------------------------------------------

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

pub fn page_dir_segment(page_id: &str, _title: &str) -> String {
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

fn block_anchor(block_id: &str) -> String {
    format!(r#"<a id="b-{}"></a>"#, short_id(block_id))
}

fn yaml_scalar(v: &Value) -> String {
    if v.is_null() {
        return "null".into();
    }
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ':' | '#' | '\n' | '"' | '\''))
        || s != s.trim();
    if needs_quote {
        // serde_json::to_string handles escaping like Python's json.dumps.
        serde_json::to_string(&s).unwrap_or_else(|_| s.clone())
    } else {
        s
    }
}

fn yaml_scalar_str(v: &str) -> String {
    yaml_scalar(&Value::String(v.to_string()))
}

// ----- rich-text → markdown ------------------------------------------

fn rich_text_plain(rt: Option<&Value>) -> String {
    let Some(arr) = rt.and_then(|v| v.as_array()) else {
        return String::new();
    };
    arr.iter()
        .filter_map(|span| span.get("plain_text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

fn wrap_annotations(text: &str, ann: Option<&Value>) -> String {
    let Some(ann) = ann else {
        return text.into();
    };
    if text.is_empty() {
        return String::new();
    }
    let mut t: String = text.into();
    let truthy = |k: &str| ann.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if truthy("code") {
        t = format!("`{t}`");
    }
    if truthy("bold") {
        t = format!("**{t}**");
    }
    if truthy("italic") {
        t = format!("*{t}*");
    }
    if truthy("strikethrough") {
        t = format!("~~{t}~~");
    }
    if truthy("underline") {
        t = format!("<u>{t}</u>");
    }
    let color = ann.get("color").and_then(|v| v.as_str()).unwrap_or("");
    if !color.is_empty() && color != "default" {
        t = format!("<span style='color:{color}'>{t}</span>");
    }
    t
}

fn render_rich_text(
    rt: Option<&Value>,
    user_names: &HashMap<String, String>,
    page_titles: &HashMap<String, String>,
) -> String {
    let Some(arr) = rt.and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for span in arr {
        let t = span.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let plain = span
            .get("plain_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ann = span.get("annotations");
        if t == "mention" {
            let m = span.get("mention").cloned().unwrap_or(Value::Null);
            let mtype = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let rendered = match mtype {
                "user" => {
                    let uid = m
                        .get("user")
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let name = user_names
                        .get(uid)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            if !plain.is_empty() {
                                plain.to_string()
                            } else {
                                uid.chars().take(8).collect()
                            }
                        });
                    format!("@{name}")
                }
                "page" => {
                    let pid = m
                        .get("page")
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let title = page_titles
                        .get(pid)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            if !plain.is_empty() {
                                plain.to_string()
                            } else {
                                "(untitled page)".to_string()
                            }
                        });
                    format!("[{title}]({})", notion_url(pid))
                }
                "database" => {
                    let did = m
                        .get("database")
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let title = page_titles
                        .get(did)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            if !plain.is_empty() {
                                plain.to_string()
                            } else {
                                "(untitled db)".to_string()
                            }
                        });
                    format!("[{title}]({})", notion_url(did))
                }
                "date" => {
                    let start = m
                        .get("date")
                        .and_then(|d| d.get("start"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !start.is_empty() {
                        start.into()
                    } else {
                        plain.into()
                    }
                }
                "link_preview" => {
                    let url = m
                        .get("link_preview")
                        .and_then(|d| d.get("url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let label = if !plain.is_empty() { plain } else { url };
                    format!("[{label}]({url})")
                }
                _ => plain.into(),
            };
            out.push_str(&wrap_annotations(&rendered, ann));
            continue;
        }
        if t == "equation" {
            let expr = span
                .get("equation")
                .and_then(|e| e.get("expression"))
                .and_then(|v| v.as_str())
                .unwrap_or(plain);
            out.push('$');
            out.push_str(expr);
            out.push('$');
            continue;
        }
        let href = span.get("href").and_then(|v| v.as_str()).unwrap_or("");
        if !href.is_empty() {
            out.push_str(&wrap_annotations(&format!("[{plain}]({href})"), ann));
        } else {
            out.push_str(&wrap_annotations(plain, ann));
        }
    }
    out
}

// ----- block dispatch -----------------------------------------------

fn block_payload(block: &Value) -> &Value {
    static NULL: Lazy<Value> = Lazy::new(|| Value::Null);
    let Some(t) = block.get("type").and_then(|v| v.as_str()) else {
        return &NULL;
    };
    block.get(t).unwrap_or(&NULL)
}

fn media_url(payload: &Value) -> String {
    let ext = payload
        .get("external")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !ext.is_empty() {
        return ext.into();
    }
    payload
        .get("file")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .into()
}

struct RenderCtx<'a> {
    children_by_parent: &'a HashMap<String, Vec<Value>>,
    user_names: &'a HashMap<String, String>,
    page_titles: &'a HashMap<String, String>,
    sub_pages_dir: &'a HashMap<String, String>,
    media_urls: &'a HashMap<String, String>,
    bookmark_titles: &'a HashMap<String, String>,
    /// Per-block markdown-relative paths (e.g. `blobs/foo.png`) for
    /// image blobs we wrote to disk next to the page's `index.md`.
    /// Wins over the upstream `media_url` when present.
    local_blob_paths: &'a HashMap<String, String>,
}

fn render_block(block: &Value, ctx: &RenderCtx<'_>, depth: usize) -> Vec<String> {
    let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let payload = block_payload(block);
    let indent = "    ".repeat(depth);
    let block_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut lines: Vec<String> = vec![block_anchor(block_id)];

    let rt = |field: &str| -> String {
        render_rich_text(payload.get(field), ctx.user_names, ctx.page_titles)
    };

    let recurse = |extra_depth: usize| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(children) = ctx.children_by_parent.get(block_id) {
            for ch in children {
                out.extend(render_block(ch, ctx, depth + extra_depth));
            }
        }
        out
    };

    match btype {
        "paragraph" => {
            let text = rt("rich_text");
            if !text.is_empty() {
                lines.push(format!("{indent}{text}"));
            }
            lines.push(String::new());
            lines.extend(recurse(1));
        }
        "heading_1" => {
            lines.push(format!("# {}", rt("rich_text")));
            lines.push(String::new());
        }
        "heading_2" => {
            lines.push(format!("## {}", rt("rich_text")));
            lines.push(String::new());
        }
        "heading_3" => {
            lines.push(format!("### {}", rt("rich_text")));
            lines.push(String::new());
        }
        "bulleted_list_item" => {
            lines.push(format!("{indent}- {}", rt("rich_text")));
            lines.extend(recurse(1));
        }
        "numbered_list_item" => {
            lines.push(format!("{indent}1. {}", rt("rich_text")));
            lines.extend(recurse(1));
        }
        "to_do" => {
            let checked = payload
                .get("checked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let box_ = if checked { "[x]" } else { "[ ]" };
            lines.push(format!("{indent}- {box_} {}", rt("rich_text")));
            lines.extend(recurse(1));
        }
        "toggle" => {
            lines.push(format!(
                "{indent}<details><summary>{}</summary>",
                rt("rich_text")
            ));
            lines.push(String::new());
            lines.extend(recurse(1));
            lines.push(format!("{indent}</details>"));
            lines.push(String::new());
        }
        "quote" => {
            lines.push(format!("> {}", rt("rich_text")));
            lines.push(String::new());
            lines.extend(recurse(1));
        }
        "callout" => {
            let icon = payload
                .get("icon")
                .and_then(|v| v.get("emoji"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("💡")
                .to_string();
            lines.push(format!("> {icon} {}", rt("rich_text")));
            lines.push(String::new());
            lines.extend(recurse(1));
        }
        "code" => {
            let lang = payload
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let text = rich_text_plain(payload.get("rich_text"));
            lines.push(format!("```{lang}"));
            lines.push(text);
            lines.push("```".into());
            let caption = rt("caption");
            if !caption.is_empty() {
                lines.push(String::new());
                lines.push(format!("*{caption}*"));
            }
            lines.push(String::new());
        }
        "divider" => {
            lines.push("---".into());
            lines.push(String::new());
        }
        "image" => {
            // Prefer the locally-cached blob (written next to the page's
            // index.md by render_one_page) so the markdown stays
            // displayable even after Notion's signed URLs expire. Fall
            // back to the upstream URL when we don't have the bytes.
            let local = ctx.local_blob_paths.get(block_id).cloned();
            let mut url = local.unwrap_or_default();
            if url.is_empty() {
                url = media_url(payload);
            }
            if url.is_empty() {
                url = ctx.media_urls.get(block_id).cloned().unwrap_or_default();
            }
            let caption_raw = rt("caption");
            let caption = if caption_raw.is_empty() {
                "image".to_string()
            } else {
                caption_raw
            };
            if !url.is_empty() {
                lines.push(format!("![{caption}]({url})"));
            } else {
                lines.push(format!("*(image: {caption})*"));
            }
            lines.push(String::new());
        }
        "video" | "audio" | "pdf" | "file" => {
            let mut url = media_url(payload);
            if url.is_empty() {
                url = ctx.media_urls.get(block_id).cloned().unwrap_or_default();
            }
            let caption_raw = rt("caption");
            let caption = if caption_raw.is_empty() {
                btype.to_string()
            } else {
                caption_raw
            };
            let name = if btype == "file" {
                payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| caption.clone())
            } else {
                caption.clone()
            };
            let icon = match btype {
                "video" => "🎬",
                "audio" => "🎵",
                "pdf" => "📄",
                _ => "📎",
            };
            if !url.is_empty() {
                lines.push(format!("[{icon} {name}]({url})"));
            } else {
                lines.push(format!("*({btype}: {name})*"));
            }
            lines.push(String::new());
        }
        "embed" => {
            let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let caption = rt("caption");
            let label = if !caption.is_empty() { &caption } else { url };
            lines.push(format!("[{label}]({url})"));
            lines.push(String::new());
        }
        "bookmark" => {
            let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let mut caption = rt("caption");
            if caption.is_empty() {
                caption = ctx
                    .bookmark_titles
                    .get(block_id)
                    .cloned()
                    .unwrap_or_default();
            }
            let label = if !caption.is_empty() {
                caption.clone()
            } else {
                url.into()
            };
            lines.push(format!("[{label}]({url})"));
            lines.push(String::new());
        }
        "link_preview" => {
            let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!("🔗 [{url}]({url})"));
            lines.push(String::new());
        }
        "link_to_page" => {
            let target_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let target_id = if !target_type.is_empty() {
                payload
                    .get(target_type)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            } else {
                ""
            };
            let title = ctx
                .page_titles
                .get(target_id)
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(linked page)".into());
            let href = if !target_id.is_empty() {
                if let Some(seg) = ctx.sub_pages_dir.get(target_id) {
                    format!("../{seg}/index.md")
                } else {
                    notion_url(target_id)
                }
            } else {
                "#".into()
            };
            lines.push(format!("{indent}🔗 [{title}]({href})"));
            lines.push(String::new());
        }
        "child_page" => {
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("(untitled)")
                .to_string();
            let seg = ctx
                .sub_pages_dir
                .get(block_id)
                .cloned()
                .unwrap_or_else(|| page_dir_segment(block_id, &title));
            let href = format!("../{seg}/index.md");
            lines.push(format!("{indent}- 📄 [{title}]({href})"));
            lines.push(String::new());
        }
        "child_database" => {
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("(database)")
                .to_string();
            lines.push(format!("{indent}*[📊 Database: {title}]*"));
            lines.push(String::new());
        }
        "synced_block" => {
            let synced_from = payload.get("synced_from");
            if synced_from.is_none() || synced_from.map(|v| v.is_null()).unwrap_or(true) {
                lines.extend(recurse(0));
            } else {
                let src_id = synced_from
                    .and_then(|v| v.get("block_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                lines.push(format!("{indent}<!-- synced from {src_id} -->"));
                lines.extend(recurse(0));
            }
        }
        "column_list" | "column" => {
            lines.extend(recurse(0));
        }
        "table" => {
            let has_header = payload
                .get("has_column_header")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut rendered_rows: Vec<Vec<String>> = Vec::new();
            if let Some(rows) = ctx.children_by_parent.get(block_id) {
                for r in rows {
                    if r.get("type").and_then(|v| v.as_str()) != Some("table_row") {
                        continue;
                    }
                    let cells = r
                        .get("table_row")
                        .and_then(|v| v.get("cells"))
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    rendered_rows.push(
                        cells
                            .iter()
                            .map(|c| render_rich_text(Some(c), ctx.user_names, ctx.page_titles))
                            .collect(),
                    );
                }
            }
            if !rendered_rows.is_empty() {
                let ncols = rendered_rows.iter().map(|r| r.len()).max().unwrap_or(0);
                let header = if has_header {
                    rendered_rows[0].clone()
                } else {
                    vec![String::new(); ncols]
                };
                lines.push(format!("| {} |", header.join(" | ")));
                lines.push(format!("| {} |", vec!["---"; ncols].join(" | ")));
                let body_rows: &[Vec<String>] = if has_header {
                    &rendered_rows[1..]
                } else {
                    &rendered_rows[..]
                };
                for r in body_rows {
                    lines.push(format!("| {} |", r.join(" | ")));
                }
                lines.push(String::new());
            }
        }
        "table_of_contents" => {
            lines.push("*[table of contents]*".into());
            lines.push(String::new());
        }
        "breadcrumb" => {
            lines.push("*[breadcrumb]*".into());
            lines.push(String::new());
        }
        "equation" => {
            let expr = payload
                .get("expression")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!("$$ {expr} $$"));
            lines.push(String::new());
        }
        "unsupported" => {
            lines.push("*[unsupported block type]*".into());
            lines.push(String::new());
        }
        _ => {
            let text = rt("rich_text");
            if !text.is_empty() {
                lines.push(format!("{indent}{text}"));
            } else {
                lines.push(format!("{indent}*[unhandled block: {btype}]*"));
            }
            lines.push(String::new());
            tracing::warn!(block_type = btype, "unhandled official-API block type");
        }
    }
    lines
}

// ----- page title resolution + tree indexing ----------------------

fn page_title(page: &Value) -> String {
    let props = page.get("properties").cloned().unwrap_or(Value::Null);
    if let Some(obj) = props.as_object() {
        for prop in obj.values() {
            if prop.get("type").and_then(|v| v.as_str()) == Some("title") {
                let t = rich_text_plain(prop.get("title"));
                return if t.is_empty() { "(untitled)".into() } else { t };
            }
        }
    }
    "(untitled)".into()
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
            out.insert(id, page_title(p));
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
        out.entry(id.to_string()).or_insert(title);
    }
    out
}

fn index_children(blocks: &[Value]) -> HashMap<String, Vec<Value>> {
    let mut by_parent: HashMap<String, Vec<Value>> = HashMap::new();
    for b in blocks {
        let parent = b.get("parent").cloned().unwrap_or(Value::Null);
        let ptype = parent.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let pid = match ptype {
            "page_id" => parent.get("page_id").and_then(|v| v.as_str()).unwrap_or(""),
            "block_id" => parent
                .get("block_id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            _ => "",
        };
        if pid.is_empty() {
            continue;
        }
        by_parent
            .entry(pid.to_string())
            .or_default()
            .push(b.clone());
    }
    by_parent
}

fn block_to_page_id(blocks: &[Value]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for b in blocks {
        let parent = b.get("parent").cloned().unwrap_or(Value::Null);
        if parent.get("type").and_then(|v| v.as_str()) == Some("page_id") {
            let bid = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let pid = parent.get("page_id").and_then(|v| v.as_str()).unwrap_or("");
            if !bid.is_empty() && !pid.is_empty() {
                out.insert(bid.into(), pid.into());
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
            let par = b.get("parent").cloned().unwrap_or(Value::Null);
            if par.get("type").and_then(|v| v.as_str()) == Some("block_id") {
                let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let pp = par.get("block_id").and_then(|v| v.as_str()).unwrap_or("");
                if !id.is_empty() && !pp.is_empty() {
                    block_parent.insert(id.into(), pp.into());
                }
            }
        }
        let mut cur = Some(bid.to_string());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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

// ----- top-level rendering -----------------------------------------

fn collect_sub_pages_dir(
    pid: &str,
    children_by_parent: &HashMap<String, Vec<Value>>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut stack: Vec<Value> = children_by_parent.get(pid).cloned().unwrap_or_default();
    while let Some(b) = stack.pop() {
        if b.get("type").and_then(|v| v.as_str()) == Some("child_page") {
            let bid = b
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = b
                .get("child_page")
                .and_then(|v| v.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.insert(bid.clone(), page_dir_segment(&bid, &title));
        } else {
            let bid = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(ch) = children_by_parent.get(bid) {
                stack.extend(ch.iter().cloned());
            }
        }
    }
    out
}

fn write_text_trim(path: &Path, parts: &[String]) -> Result<()> {
    let mut s = parts.join("\n");
    while s.ends_with(|c: char| c.is_whitespace()) {
        s.pop();
    }
    s.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_one_page(
    page: &Value,
    children_by_parent: &HashMap<String, Vec<Value>>,
    user_names: &HashMap<String, String>,
    page_titles: &HashMap<String, String>,
    media_urls: &HashMap<String, String>,
    bookmark_titles: &HashMap<String, String>,
    blobs: &dyn BlobReader,
    pages_root: &Path,
) -> Result<PathBuf> {
    let pid = page.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let title = page_titles
        .get(pid)
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".into());
    let seg = page_dir_segment(pid, &title);
    let page_dir = pages_root.join(&seg);
    fs::create_dir_all(&page_dir)?;
    let sub_pages_dir = collect_sub_pages_dir(pid, children_by_parent);

    let mut parts: Vec<String> = Vec::new();
    parts.push("---".into());
    parts.push("provider: notion_official".into());
    parts.push(format!("page_id: {}", yaml_scalar_str(pid)));
    parts.push(format!("title: {}", yaml_scalar_str(&title)));
    parts.push(format!(
        "created_time: {}",
        yaml_scalar(page.get("created_time").unwrap_or(&Value::Null))
    ));
    parts.push(format!(
        "last_edited_time: {}",
        yaml_scalar(page.get("last_edited_time").unwrap_or(&Value::Null))
    ));
    parts.push("---".into());
    parts.push(String::new());
    let icon = page
        .get("icon")
        .and_then(|v| v.get("emoji"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let icon_prefix = if !icon.is_empty() {
        format!("{icon} ")
    } else {
        String::new()
    };
    let title_text = format!("{icon_prefix}{title}");
    let url = notion_url(pid);
    parts.push(
        Title {
            text: &title_text,
            markdown_uuid: Some(pid),
            source_url: Some(&url),
        }
        .render()
        .trim_end()
        .to_string(),
    );
    parts.push(String::new());

    // Materialize every blob that hangs off a block on this page.
    // Files land in `<page_dir>/blobs/<short-b3>.<ext>`; the relative
    // path `blobs/<file>` is the one we splice into the markdown.
    //
    // Streams from the [`BlobReader`] one block at a time — peak RSS
    // stays at a single attachment instead of holding the whole
    // page's blob set in memory.
    let mut local_blob_paths: HashMap<String, String> = HashMap::new();
    if let Some(blocks) = children_by_parent.get(pid) {
        let mut owner_ids: Vec<String> = Vec::new();
        collect_block_ids(blocks, children_by_parent, &mut owner_ids);
        let blobs_dir = page_dir.join("blobs");
        let mut created_dir = false;
        for owner in owner_ids {
            let Some(b) = blobs.read_by_owner(&owner)? else {
                continue;
            };
            if !created_dir {
                fs::create_dir_all(&blobs_dir)?;
                created_dir = true;
            }
            let filename = b.rendered_filename();
            let target = blobs_dir.join(&filename);
            // Trust our copy: write only if the file isn't already
            // present with the same length. Lets `notion-translate`
            // re-runs stay cheap.
            let needs_write = match fs::metadata(&target) {
                Ok(m) => m.len() as usize != b.bytes.len(),
                Err(_) => true,
            };
            if needs_write {
                fs::write(&target, &b.bytes)?;
            }
            local_blob_paths.insert(owner, format!("blobs/{filename}"));
        }
    }

    let ctx = RenderCtx {
        children_by_parent,
        user_names,
        page_titles,
        sub_pages_dir: &sub_pages_dir,
        media_urls,
        bookmark_titles,
        local_blob_paths: &local_blob_paths,
    };
    if let Some(children) = children_by_parent.get(pid) {
        for ch in children {
            parts.extend(render_block(ch, &ctx, 0));
        }
    }

    let target = page_dir.join("index.md");
    write_text_trim(&target, &parts)?;
    Ok(target)
}

/// Walk every block reachable from `roots` via `children_by_parent`,
/// collecting block ids. Used to find candidates in `blobs_by_owner`.
fn collect_block_ids(
    roots: &[Value],
    children_by_parent: &HashMap<String, Vec<Value>>,
    out: &mut Vec<String>,
) {
    for b in roots {
        if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
            out.push(id.to_string());
            if let Some(kids) = children_by_parent.get(id) {
                collect_block_ids(kids, children_by_parent, out);
            }
        }
    }
}

pub fn thread_snippet(comment_rich_text_plain: &str) -> String {
    let first_line = comment_rich_text_plain
        .lines()
        .next()
        .unwrap_or("thread")
        .to_string();
    let s = if first_line.is_empty() {
        "thread".into()
    } else {
        first_line
    };
    s.chars().take(60).collect()
}

pub fn thread_filename(discussion_id: &str, _snippet: &str) -> String {
    format!("{discussion_id}.md")
}

#[allow(clippy::too_many_arguments)]
fn render_thread(
    discussion_id: &str,
    page_id: &str,
    page_title_str: &str,
    parent_block_id: Option<&str>,
    comments: &[Value],
    user_names: &HashMap<String, String>,
    page_titles: &HashMap<String, String>,
    page_dir: &Path,
) -> Result<Option<PathBuf>> {
    if comments.is_empty() {
        return Ok(None);
    }
    let mut sorted: Vec<Value> = comments.to_vec();
    sorted.sort_by(|a, b| {
        let aa = a.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
        let bb = b.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
        aa.cmp(bb)
    });
    let first_text = rich_text_plain(sorted[0].get("rich_text"));
    let snippet = thread_snippet(&first_text);
    let threads_dir = page_dir.join("threads");
    fs::create_dir_all(&threads_dir)?;
    let target = threads_dir.join(thread_filename(discussion_id, &snippet));

    let thread_url = notion_thread_url(page_id, Some(discussion_id), parent_block_id);

    let mut parts: Vec<String> = Vec::new();
    parts.push("---".into());
    parts.push("provider: notion_official".into());
    parts.push(format!("discussion_id: {}", yaml_scalar_str(discussion_id)));
    parts.push(format!("page_id: {}", yaml_scalar_str(page_id)));
    if let Some(pb) = parent_block_id {
        parts.push(format!("parent_block_id: {}", yaml_scalar_str(pb)));
    }
    parts.push("---".into());
    parts.push(String::new());
    // Shared `Title` helper. `markdown_uuid` is the notion
    // discussion id (matches `discussion_id:` in the frontmatter and
    // the grid_row uuid for this thread); `source_url` is the
    // `notion.so/{page}?d={discussion}#{anchor}` deep link, which
    // collapses the previously-separate "# Comment thread …" H1 +
    // "View thread on Notion ↗" bullet into a single block.
    let title_text = format!("Comment thread on “{page_title_str}”");
    parts.push(
        frankweiler_etl::title::Title {
            text: &title_text,
            markdown_uuid: Some(discussion_id),
            source_url: Some(&thread_url),
        }
        .render()
        .trim_end()
        .to_string(),
    );
    parts.push(String::new());
    if let Some(pb) = parent_block_id {
        let anchor = format!("../index.md#b-{}", short_id(pb));
        parts.push(format!("Anchored to [block ↩]({anchor})"));
        parts.push(String::new());
    }

    for c in &sorted {
        let cid = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let author_id = c
            .get("created_by")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let author = user_names
            .get(author_id)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let s: String = author_id.chars().take(8).collect();
                if s.is_empty() {
                    "unknown".into()
                } else {
                    s
                }
            });
        let created = c.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
        parts.push(format!(r#"<a id="c-{}"></a>"#, short_id(cid)));
        parts.push(String::new());
        parts.push(format!("## {author}"));
        parts.push(String::new());
        parts.push(format!("*{created}* — [↗]({thread_url})"));
        parts.push(String::new());
        let body = render_rich_text(c.get("rich_text"), user_names, page_titles);
        parts.push(body);
        parts.push(String::new());
    }
    write_text_trim(&target, &parts)?;
    Ok(Some(target))
}

pub fn pages_subdir() -> PathBuf {
    PathBuf::from("rendered_md").join("notion").join("pages")
}

pub fn page_qmd_path_rel(page_id: &str, page_title_str: &str) -> String {
    let seg = page_dir_segment(page_id, page_title_str);
    pages_subdir()
        .join(seg)
        .join("index.md")
        .to_string_lossy()
        .into_owned()
}

pub fn thread_qmd_path_rel(
    page_id: &str,
    page_title_str: &str,
    discussion_id: &str,
    snippet: &str,
) -> String {
    let seg = page_dir_segment(page_id, page_title_str);
    pages_subdir()
        .join(seg)
        .join("threads")
        .join(thread_filename(discussion_id, snippet))
        .to_string_lossy()
        .into_owned()
}

pub fn render_notion_official(
    parsed: &ParsedNotionOfficial,
    root: &Path,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    let pages_root = root.join(pages_subdir());
    fs::create_dir_all(&pages_root)?;

    if parsed.pages.is_empty() && parsed.blocks.is_empty() && parsed.comments.is_empty() {
        return Ok(summary);
    }
    let children_by_parent = index_children(&parsed.blocks);
    let page_titles = build_page_titles(&parsed.pages, &parsed.blocks);
    let pages_by_id: HashMap<String, Value> = parsed
        .pages
        .iter()
        .filter_map(|p| {
            p.get("id")
                .and_then(|v| v.as_str())
                .map(|id| (id.to_string(), p.clone()))
        })
        .collect();
    let block_owning_page = block_to_page_id(&parsed.blocks);

    // Pre-compute every document's row set + fingerprint up front so
    // the per-doc loop can decide skip/render against priors before
    // doing any IO.
    let docs = gather_documents(parsed);
    let page_doc_by_uuid: HashMap<String, &PageDocument> = docs
        .pages
        .iter()
        .map(|d| (d.page_uuid.clone(), d))
        .collect();
    let thread_doc_by_uuid: HashMap<String, &ThreadDocument> = docs
        .threads
        .iter()
        .map(|d| (d.discussion_uuid.clone(), d))
        .collect();

    progress.set_length(Some((docs.pages.len() + docs.threads.len()) as u64));

    // ── Pages ────────────────────────────────────────────────────────
    // Compute every page's md_path even if we end up skipping it, so
    // threads-under-the-page can resolve their parent dir.
    let mut page_paths: HashMap<String, PathBuf> = HashMap::new();
    for page in &parsed.pages {
        let Some(pid) = page.get("id").and_then(|v| v.as_str()).map(String::from) else {
            continue;
        };
        let title = page_titles
            .get(&pid)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(untitled)".into());
        let page_dir = pages_root.join(page_dir_segment(&pid, &title));
        let md_path = page_dir.join("index.md");

        let fingerprint = page_doc_by_uuid
            .get(&pid)
            .map(|d| d.source_fingerprint.clone());

        if let Some(fp) = fingerprint.as_ref() {
            if prior_fingerprints.get(&pid) == Some(fp) && md_path.exists() {
                summary.skipped += 1;
                page_paths.insert(pid, md_path);
                progress.inc(1);
                continue;
            }
        }

        let target = render_one_page(
            page,
            &children_by_parent,
            &parsed.user_names,
            &page_titles,
            &parsed.media_urls,
            &parsed.bookmark_titles,
            parsed.blobs.as_ref(),
            &pages_root,
        )?;

        // Sidecar + callback only fire if gather_documents knew about
        // this page (it should, for any page that produced rows).
        if let Some(pd) = page_doc_by_uuid.get(&pid) {
            let sidecar = Sidecar {
                header: SidecarHeader {
                    markdown_uuid: pd.page_uuid.clone(),
                    source_fingerprint: pd.source_fingerprint.clone(),
                    render_version: RENDER_VERSION,
                },
                rows: pd.rows.clone(),
                edges: Vec::new(),
            };
            let sidecar_path = target.with_extension("grid_rows.json");
            let json = serde_json::to_string_pretty(&sidecar)?;
            fs::write(&sidecar_path, json)?;

            on_doc_complete(RenderedMarkdown {
                markdown_uuid: pd.page_uuid.clone(),
                source_name: String::new(),
                source_fingerprint: pd.source_fingerprint.clone(),
                upstream_cursor: None,
                md_path: target.clone(),
                render_version: RENDER_VERSION,
                rows: pd.rows.clone(),
                edges: Vec::new(),
            })?;
        }

        page_paths.insert(pid, target);
        summary.rendered += 1;
        progress.inc(1);
    }

    // ── Comment threads ──────────────────────────────────────────────
    let mut by_disc: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for c in &parsed.comments {
        let did = c
            .get("discussion_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if did.is_empty() {
            continue;
        }
        by_disc.entry(did.to_string()).or_default().push(c.clone());
    }
    for (disc_id, mut members) in by_disc {
        members.sort_by(|a, b| {
            let aa = a.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
            let bb = b.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
            aa.cmp(bb)
        });
        let first = &members[0];
        let Some(page_id) = resolve_comment_page_id(first, &parsed.blocks, &block_owning_page)
        else {
            continue;
        };
        let Some(_page) = pages_by_id.get(&page_id) else {
            continue;
        };
        let title = page_titles
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
        let page_dir = pages_root.join(page_dir_segment(&page_id, &title));

        let fingerprint = thread_doc_by_uuid
            .get(&disc_id)
            .map(|d| d.source_fingerprint.clone());

        // We need the rendered thread's md_path to do the on-disk
        // skip check. render_thread is deterministic and
        // thread_filename ignores the snippet (PK is discussion_id),
        // so we can call thread_qmd_path_rel with an empty snippet
        // and still get the right path.
        let thread_md_rel = thread_qmd_path_rel(&page_id, &title, &disc_id, "");
        let thread_md_path = root.join(&thread_md_rel);

        if let Some(fp) = fingerprint.as_ref() {
            if prior_fingerprints.get(&disc_id) == Some(fp) && thread_md_path.exists() {
                summary.skipped += 1;
                progress.inc(1);
                continue;
            }
        }

        let Some(p) = render_thread(
            &disc_id,
            &page_id,
            &title,
            parent_block_id.as_deref(),
            &members,
            &parsed.user_names,
            &page_titles,
            &page_dir,
        )?
        else {
            continue;
        };

        if let Some(td) = thread_doc_by_uuid.get(&disc_id) {
            let sidecar = Sidecar {
                header: SidecarHeader {
                    markdown_uuid: td.discussion_uuid.clone(),
                    source_fingerprint: td.source_fingerprint.clone(),
                    render_version: RENDER_VERSION,
                },
                rows: td.rows.clone(),
                edges: Vec::new(),
            };
            let sidecar_path = p.with_extension("grid_rows.json");
            let json = serde_json::to_string_pretty(&sidecar)?;
            fs::write(&sidecar_path, json)?;

            on_doc_complete(RenderedMarkdown {
                markdown_uuid: td.discussion_uuid.clone(),
                source_name: String::new(),
                source_fingerprint: td.source_fingerprint.clone(),
                upstream_cursor: None,
                md_path: p.clone(),
                render_version: RENDER_VERSION,
                rows: td.rows.clone(),
                edges: Vec::new(),
            })?;
        }

        summary.rendered += 1;
        progress.inc(1);
    }

    Ok(summary)
}
