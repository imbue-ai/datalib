//! Notion's enhanced markdown, as its API returns a page body, turned
//! into the CommonMark the viewer and the search index read.
//!
//! Notion writes one block per line with no blank line between them, and
//! nests a block's children one tab deeper than the block. Read as
//! CommonMark that is one run-on paragraph, every tab-indented child is
//! an indented code block, and a tag Notion invented (`<empty-block/>`,
//! `<callout>`) opens a raw HTML block that swallows the markdown after
//! it. So each block gets its own paragraph, a child sits at its parent's
//! level (inside it, for a list item or a callout), and every Notion tag
//! becomes the markdown or the HTML the viewer allows.

use std::collections::HashMap;
use std::sync::LazyLock;

use datalib_etl::ids::normalize_id_token;
use datalib_etl_notion::ingest::db::TreeRow;
use datalib_etl_notion::ingest::format_uuid;
use datalib_etl_notion::ingest::schema_raw::TreeKind;
use datalib_etl_render::html::{
    code_span_parts, escape_attr, escape_md_inline, escape_text, md_link_dest,
};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::message::{chip_link, stamp_text_html};
use regex::{Captures, Regex};

use super::grid_rows::user_handle;

/// What a body's mentions and links are resolved against.
#[derive(Clone, Copy)]
pub(crate) struct Context<'a> {
    pub stanza: &'a str,
    pub tree: Lookup<'a, HashMap<String, TreeRow>>,
    pub users: Lookup<'a, HashMap<String, String>>,
    pub emails: Lookup<'a, HashMap<String, String>>,
}

impl Context<'_> {
    /// Where a link to a Notion page or database goes, and its title: the
    /// page's document when this source renders it, else Notion.
    fn target(&self, url: &str) -> (String, Option<String>) {
        let id = format_uuid(&normalize_id_token(url));
        match self.tree.get(&id) {
            Some(row) => {
                let href = match row.kind {
                    Some(TreeKind::Page) => {
                        let uuid =
                            super::ids::page(self.stanza, &id, row.created_time.as_deref()).uuid;
                        format!("/chat/{uuid}")
                    }
                    _ => url.to_string(),
                };
                (href, row.title.clone())
            }
            None => (url.to_string(), None),
        }
    }

    fn user(&self, url: &str) -> String {
        let uid = url.strip_prefix("user://").unwrap_or(url);
        let name = self.users.get(uid).map(String::as_str).unwrap_or("");
        match user_handle(uid, self.emails) {
            Some(h) => chip_link(name, &h),
            None => format!("@{}", escape_text(name)),
        }
    }
}

pub(crate) fn to_markdown(src: &str, cx: &Context<'_>) -> String {
    join(&blocks(src, cx))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    ListItem,
    Other,
}

/// One block of output: its lines, each written after `prefix`, the
/// indentation or quote markers of the blocks it sits in.
struct Block {
    prefix: String,
    kind: Kind,
    lines: Vec<String>,
}

/// A block whose children may follow, at `depth` tabs.
struct Frame {
    depth: usize,
    child_prefix: String,
    /// A callout's icon, for its first child to lead with.
    icon: Option<String>,
}

fn blocks(src: &str, cx: &Context<'_>) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        i += 1;
        let text = raw.trim_start_matches('\t');
        let depth = raw.len() - text.len();
        let text = strip_block_attrs(text.trim_end());
        if text.is_empty() {
            continue;
        }
        while stack.last().is_some_and(|f| f.depth >= depth) {
            stack.pop();
        }
        let prefix = stack
            .last()
            .map(|f| f.child_prefix.clone())
            .unwrap_or_default();
        let icon = stack.last_mut().and_then(|f| f.icon.take());
        let mut frame = Frame {
            depth,
            child_prefix: prefix.clone(),
            icon: None,
        };
        let mut emit: Option<(Kind, Vec<String>)> = None;

        // A fence's body is written as is, unindented whatever its depth.
        if let Some(fence) = fence_of(text) {
            let mut body = vec![text.to_string()];
            while i < lines.len() {
                let line = lines[i];
                i += 1;
                let bare = line.trim_start_matches('\t').trim_end();
                if closes_fence(bare, fence) {
                    body.push(bare.to_string());
                    break;
                }
                body.push(line.to_string());
            }
            emit = Some((Kind::Other, body));
        } else if text.starts_with("<table") && !text.starts_with("<table_of_contents") {
            // So are a table's rows.
            let mut rows = Vec::new();
            while i < lines.len() {
                let line = lines[i].trim();
                i += 1;
                if line == "</table>" {
                    break;
                }
                rows.push(line);
            }
            emit = Some((Kind::Other, pipe_table(text, &rows, cx)));
        } else if text == "<empty-block/>" || text.starts_with("<table_of_contents") {
            continue;
        } else if text == "<details>" || text.starts_with("<details ") {
            emit = Some((Kind::Other, vec!["<details>".into()]));
        } else if text == "</details>" {
            emit = Some((Kind::Other, vec!["</details>".into()]));
        } else if let Some(inner) = text
            .strip_prefix("<summary>")
            .and_then(|t| t.strip_suffix("</summary>"))
        {
            let summary = format!("<summary>{}</summary>", summary_html(inner, cx));
            emit = Some((Kind::Other, vec![summary]));
        } else if let Some(c) = CALLOUT_ONE_LINE.captures(text) {
            let lead = attr(c.get(1).map_or("", |m| m.as_str()), "icon")
                .map(|i| format!("{i} "))
                .unwrap_or_default();
            emit = Some((Kind::Other, vec![format!("> {lead}{}", inline(&c[2], cx))]));
        } else if let Some(c) = CALLOUT_OPEN.captures(text) {
            frame.child_prefix = format!("{prefix}> ");
            frame.icon = attr(c.get(1).map_or("", |m| m.as_str()), "icon");
        } else if let Some(line) = leaf(text, cx) {
            emit = Some((Kind::Other, vec![line]));
        } else if LEAF.is_match(text) || (CONTAINER_TAG.is_match(text) && !MENTION.is_match(text)) {
            // `<columns>`, `<synced_block>`, `</callout>`, a button: the
            // children read on at this level.
        } else {
            let (marker, rest) = split_marker(text);
            let lead = icon.map(|i| format!("{i} ")).unwrap_or_default();
            let line = format!("{marker}{lead}{}", inline(rest, cx));
            let kind = match LIST_MARKER.find(marker) {
                Some(m) => {
                    frame.child_prefix = format!("{prefix}{}", " ".repeat(m.end()));
                    Kind::ListItem
                }
                None => {
                    if marker.starts_with('>') {
                        frame.child_prefix = format!("{prefix}> ");
                    }
                    Kind::Other
                }
            };
            emit = Some((kind, vec![line]));
        }
        if let Some((kind, lines)) = emit {
            push(&mut out, prefix, kind, lines);
        }
        stack.push(frame);
    }
    out
}

/// A Notion table written as pipe rows is one block per row; the rows
/// stay together.
fn push(out: &mut Vec<Block>, prefix: String, kind: Kind, lines: Vec<String>) {
    if let (Some(prev), [line]) = (out.last_mut(), lines.as_slice()) {
        if line.starts_with('|')
            && prev.prefix == prefix
            && prev.lines.last().is_some_and(|l| l.starts_with('|'))
        {
            prev.lines.push(line.clone());
            return;
        }
    }
    out.push(Block {
        prefix,
        kind,
        lines,
    });
}

/// The blocks as one document: a blank line between two blocks, except
/// between two list items, which keeps a list tight.
fn join(blocks: &[Block]) -> String {
    let mut out = String::new();
    for (n, b) in blocks.iter().enumerate() {
        if n > 0 {
            let prev = &blocks[n - 1];
            if !(prev.kind == Kind::ListItem && b.kind == Kind::ListItem) {
                out.push_str(common_prefix(&prev.prefix, &b.prefix).trim_end());
                out.push('\n');
            }
        }
        for line in &b.lines {
            let full = format!("{}{line}", b.prefix);
            out.push_str(full.trim_end());
            out.push('\n');
        }
    }
    out
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let n = a
        .char_indices()
        .zip(b.chars())
        .take_while(|((_, x), y)| x == y)
        .last()
        .map_or(0, |((i, c), _)| i + c.len_utf8());
    &a[..n]
}

static BLOCK_ATTRS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\s*\{(?:[a-z_-]+="[^"]*"\s*)+\}$"#).unwrap());
static CALLOUT_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<callout(\s[^>]*)?>$").unwrap());
static CALLOUT_ONE_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<callout(\s[^>]*)?>(.*)</callout>$").unwrap());
static CONTAINER_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^</?[a-z][a-z_-]*(\s[^>]*)?>$").unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([a-z_-]+)="([^"]*)""#).unwrap());
static LEAF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^<(page|database|file|pdf|embed|video|audio|unknown)(\s[^>]*?)?\s*(?:/>|>(.*)</[a-z]+>)$",
    )
    .unwrap()
});
static LIST_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-*+]|\d{1,9}[.)])\s").unwrap());

/// `{toggle="true"}`, `{color="blue_bg"}`: how Notion draws the block,
/// which the viewer has no use for. A toggle heading's children are shown.
fn strip_block_attrs(line: &str) -> &str {
    BLOCK_ATTRS.find(line).map_or(line, |m| &line[..m.start()])
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    ATTR.captures_iter(attrs)
        .find(|c| &c[1] == name)
        .map(|c| {
            c[2].replace("&quot;", "\"")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .filter(|v| !v.is_empty())
}

fn fence_of(text: &str) -> Option<&str> {
    let c = text.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = &text[..text.len() - text.trim_start_matches(c).len()];
    (run.len() >= 3).then_some(run)
}

fn closes_fence(line: &str, fence: &str) -> bool {
    let c = fence.chars().next().expect("a fence is three characters");
    line.len() >= fence.len() && line.chars().all(|x| x == c)
}

/// The marker that opens a line's block, and the rest: `- `, `1. `,
/// `- [ ] `, `## `, `> `, or nothing.
fn split_marker(text: &str) -> (&str, &str) {
    let mut end = 0;
    if let Some(m) = LIST_MARKER.find(text) {
        end = m.end();
        for task in ["[ ] ", "[x] ", "[X] "] {
            if text[end..].starts_with(task) {
                end += task.len();
            }
        }
    } else if text.starts_with('#') {
        let hashes = text.len() - text.trim_start_matches('#').len();
        if hashes <= 6 && text[hashes..].starts_with(' ') {
            end = hashes + 1;
        }
    } else if text.starts_with("> ") {
        end = 2;
    }
    text.split_at(end)
}

/// A block Notion writes as a tag of its own, as one line of markdown:
/// a link for a child page, a database, a file, an embed; a player for a
/// video or a sound. A button and the table of contents draw nothing.
fn leaf(text: &str, cx: &Context<'_>) -> Option<String> {
    let c = LEAF.captures(text)?;
    let attrs = c.get(2).map_or("", |m| m.as_str());
    let inner = c.get(3).map_or("", |m| m.as_str()).trim();
    let src = attr(attrs, "url").or_else(|| attr(attrs, "src"));
    let link = |text: &str, url: &str| format!("[{text}]({})", md_link_dest(url));
    Some(match (&c[1], src) {
        ("page" | "database", Some(url)) => {
            let (href, title) = cx.target(&url);
            let text = if inner.is_empty() {
                title
                    .map(|t| escape_md_inline(&t))
                    .unwrap_or_else(|| "Untitled".into())
            } else {
                inline(inner, cx)
            };
            format!("📄 {}", link(&text, &href))
        }
        ("video" | "audio", Some(src)) => format!(
            "<{tag} controls src=\"{}\"></{tag}>",
            escape_attr(&src),
            tag = &c[1]
        ),
        ("unknown", Some(_)) if attr(attrs, "alt").as_deref() == Some("button") => return None,
        ("unknown", Some(url)) => {
            let what = attr(attrs, "alt").unwrap_or_else(|| "block".into());
            format!(
                "*{}*",
                link(&format!("Notion {}", what.replace('_', " ")), &url)
            )
        }
        (_, Some(url)) => {
            let text = if inner.is_empty() {
                "Attachment".to_string()
            } else {
                inline(inner, cx)
            };
            format!("📎 {}", link(&text, &url))
        }
        (_, None) => return None,
    })
}

static SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?span(?:\s[^>]*)?>").unwrap());
static MENTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"<(mention-date|mention-user|mention-page|mention-database|link-preview)(\s[^>]*?)?\s*(?:/>|>(.*?)</mention-[a-z]+>)",
    )
    .unwrap()
});
/// `**Problem: **What`: Notion keeps the space inside the emphasis,
/// where markdown will not close it.
static BOLD_TRAILING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*([^\s*](?:[^*]*?[^\s*])?)(\s+)\*\*").unwrap());
static ITALIC_TRAILING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^*\w])\*([^\s*](?:[^*]*?[^\s*])?)(\s+)\*([^*]|$)").unwrap());

/// One line of Notion's inline markdown, outside its code spans.
fn inline(text: &str, cx: &Context<'_>) -> String {
    code_span_parts(text)
        .into_iter()
        .map(|(part, code)| {
            if code {
                part.to_string()
            } else {
                inline_text(part, cx)
            }
        })
        .collect()
}

fn inline_text(text: &str, cx: &Context<'_>) -> String {
    let text = SPAN.replace_all(text, "");
    let text = MENTION.replace_all(&text, |c: &Captures| mention(c, cx, false));
    // Two bold runs side by side, `**a****b**`, which markdown cannot close.
    let text = text.replace("****", "");
    let text = BOLD_TRAILING_SPACE.replace_all(&text, "**$1**$2");
    ITALIC_TRAILING_SPACE
        .replace_all(&text, "$1*$2*$3$4")
        .into_owned()
}

/// A mention as markdown, or as HTML for a toggle's summary line, which
/// sits inside an HTML block where markdown is not read.
fn mention(c: &Captures, cx: &Context<'_>, html: bool) -> String {
    let attrs = c.get(2).map_or("", |m| m.as_str());
    let inner = c.get(3).map_or("", |m| m.as_str()).trim();
    let link = |text: &str, href: &str| {
        if html {
            format!(
                "<a href=\"{}\">{}</a>",
                escape_attr(href),
                escape_text(text)
            )
        } else {
            format!("[{}]({})", escape_md_inline(text), md_link_dest(href))
        }
    };
    match &c[1] {
        "mention-date" => {
            let start = attr(attrs, "start").unwrap_or_default();
            match attr(attrs, "end") {
                Some(end) => format!("{} → {}", stamp_text_html(&start), stamp_text_html(&end)),
                None => stamp_text_html(&start),
            }
        }
        "mention-user" => {
            let url = attr(attrs, "url").unwrap_or_default();
            if html {
                let uid = url.strip_prefix("user://").unwrap_or(&url);
                format!(
                    "@{}",
                    escape_text(cx.users.get(uid).map_or("", String::as_str))
                )
            } else {
                cx.user(&url)
            }
        }
        // `url` is the preview block's place in Notion; `source` is what
        // it previews.
        "link-preview" => match attr(attrs, "source") {
            Some(source) => link(&source, &source),
            None => {
                let url = attr(attrs, "url").unwrap_or_default();
                let (href, title) = cx.target(&url);
                link(title.as_deref().unwrap_or(&url), &href)
            }
        },
        _ => {
            let url = attr(attrs, "url").unwrap_or_default();
            let (href, title) = cx.target(&url);
            let text = match title {
                Some(t) => t,
                None if !inner.is_empty() => inner.to_string(),
                None => "Untitled".to_string(),
            };
            link(&text, &href)
        }
    }
}

static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\(([^)\s]*)\)").unwrap());
static MD_BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static MD_ITALIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());

/// A toggle's summary line as HTML: its mentions, code, links and
/// emphasis, and the rest escaped.
fn summary_html(text: &str, cx: &Context<'_>) -> String {
    let mut out = String::new();
    for (part, code) in code_span_parts(text) {
        if code {
            let body = part.trim_matches('`').trim();
            out.push_str(&format!("<code>{}</code>", escape_text(body)));
            continue;
        }
        let part = SPAN.replace_all(part, "");
        let mut at = 0;
        for c in MENTION.captures_iter(&part) {
            let m = c.get(0).expect("a match");
            out.push_str(&markdown_lite(&part[at..m.start()]));
            out.push_str(&mention(&c, cx, true));
            at = m.end();
        }
        out.push_str(&markdown_lite(&part[at..]));
    }
    out.trim().to_string()
}

fn markdown_lite(text: &str) -> String {
    let mut esc = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(|n| n.is_ascii_punctuation()) => {
                let n = chars.next().expect("peeked");
                esc.push_str(&format!("&#{};", n as u32));
            }
            '&' => esc.push_str("&amp;"),
            '<' => esc.push_str("&lt;"),
            '>' => esc.push_str("&gt;"),
            '"' => esc.push_str("&quot;"),
            _ => esc.push(c),
        }
    }
    let esc = MD_LINK.replace_all(&esc, "<a href=\"$2\">$1</a>");
    let esc = MD_BOLD.replace_all(&esc, "<strong>$1</strong>");
    MD_ITALIC.replace_all(&esc, "<em>$1</em>").into_owned()
}

/// A Notion `<table>` as a pipe table. A table with no header row gets
/// an empty one, since a pipe table must have one.
fn pipe_table(open: &str, rows: &[&str], cx: &Context<'_>) -> Vec<String> {
    let mut table: Vec<Vec<String>> = Vec::new();
    for line in rows {
        if *line == "<tr>" {
            table.push(Vec::new());
        } else if let Some(cell) = line
            .strip_prefix("<td>")
            .and_then(|l| l.strip_suffix("</td>"))
        {
            if let Some(row) = table.last_mut() {
                row.push(inline(cell, cx).replace('|', "\\|").replace("\\\\|", "\\|"));
            }
        }
    }
    let width = table.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return Vec::new();
    }
    let header_row = attr(open, "header-row").as_deref() == Some("true");
    let line = |cells: &[String]| {
        let mut cells: Vec<&str> = cells.iter().map(String::as_str).collect();
        cells.resize(width, "");
        format!("| {} |", cells.join(" | "))
    };
    let mut out = Vec::new();
    let mut body = table.iter();
    match header_row.then(|| body.next()).flatten() {
        Some(header) => out.push(line(header)),
        None => out.push(line(&[])),
    }
    out.push(format!("|{}", " --- |".repeat(width)));
    out.extend(body.map(|r| line(r)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::inputs::Inputs;

    fn render(src: &str) -> String {
        let tree: HashMap<String, TreeRow> = [
            (
                "1a2b3c4d-0000-4000-8000-000000001701".to_string(),
                TreeRow {
                    kind: Some(TreeKind::Page),
                    title: Some("Bridge Operations Handbook".into()),
                    parent_type: Some("workspace".into()),
                    parent_id: None,
                    created_time: None,
                },
            ),
            (
                "1a2b3c4d-0000-4000-8000-000000001702".to_string(),
                TreeRow {
                    kind: Some(TreeKind::UnmirroredPage),
                    title: Some("Ten Forward".into()),
                    parent_type: Some("workspace".into()),
                    parent_id: None,
                    created_time: None,
                },
            ),
        ]
        .into_iter()
        .collect();
        let users: HashMap<String, String> =
            [("u-riker".to_string(), "William T. Riker".to_string())].into();
        let emails: HashMap<String, String> =
            [("u-riker".to_string(), "riker@enterprise.test".to_string())].into();
        let inputs = Inputs::default();
        let cx = Context {
            stanza: "notion",
            tree: inputs.lookup("tree", &tree),
            users: inputs.lookup("users", &users),
            emails: inputs.lookup("users", &emails),
        };
        to_markdown(src, &cx)
    }

    /// A toggle heading's children are tab-indented, which markdown reads
    /// as a code block; they render as the heading's own content, and a
    /// list keeps its nesting and stays tight.
    #[test]
    fn a_toggle_headings_children_are_not_a_code_block() {
        let md = render(
            "# Mission Log {toggle=\"true\"}\n\
             \tAway team report follows.\n\
             \t- Sensors\n\
             \t\t- Long range\n\
             \t- Shields\n\
             # Next",
        );
        assert_eq!(
            md,
            "# Mission Log\n\
             \n\
             Away team report follows.\n\
             \n\
             - Sensors\n  \
               - Long range\n\
             - Shields\n\
             \n\
             # Next\n"
        );
    }

    /// Notion writes no blank line between blocks, so two paragraphs were
    /// one, and an `<empty-block/>` opened a raw HTML block that showed
    /// the markdown after it as text.
    #[test]
    fn each_block_is_its_own_paragraph_and_empty_blocks_vanish() {
        let md = render("Engage.\n<empty-block/>\n**Make it so.**");
        assert_eq!(md, "Engage.\n\n**Make it so.**\n");
    }

    #[test]
    fn emphasis_with_a_space_inside_its_close_still_closes() {
        let md = render("**Problem: **What is it? *Use as a guide. *Then **Review Notes  **");
        assert_eq!(
            md,
            "**Problem:** What is it? *Use as a guide.* Then **Review Notes**\n"
        );
        assert_eq!(
            render("- **`warp.db`****\u{a0}is the core.**"),
            "- **`warp.db`\u{a0}is the core.**\n",
            "two bold runs side by side are one"
        );
    }

    #[test]
    fn a_toggle_is_a_details_element_with_its_summary_as_html() {
        let md = render(
            "<details>\n\
             <summary><mention-date start=\"2369-04-15\"/> **Stardate** `log`</summary>\n\
             \t- Docked at Starbase 74\n\
             </details>\n\
             After",
        );
        assert!(
            md.starts_with("<details>\n\n<summary><time datetime=\"2369-04-15\">"),
            "{md}"
        );
        assert!(
            md.contains("<strong>Stardate</strong> <code>log</code></summary>\n\n- Docked at Starbase 74\n\n</details>\n\nAfter\n"),
            "{md}"
        );
    }

    #[test]
    fn mentions_link_people_pages_and_dates() {
        let md = render(
            "Ask <mention-user url=\"user://u-riker\"/> about \
             <mention-page url=\"https://app.notion.com/p/1a2b3c4d000040008000000000001701\"/> \
             and <mention-page url=\"https://app.notion.com/p/Ten-Forward-1a2b3c4d000040008000000000001702\">Ten Forward</mention-page> \
             by <mention-date start=\"2369-04-15\" end=\"2369-04-16\"/>; \
             see <link-preview url=\"https://app.notion.com/p/1a2b3c4d000040008000000000001701#b1\" \
             source=\"https://github.test/enterprise/pull/1?a=1&amp;b=2\" />",
        );
        let handbook =
            super::super::ids::page("notion", "1a2b3c4d-0000-4000-8000-000000001701", None).uuid;
        assert!(
            md.contains("[William T. Riker](mailto:riker@enterprise.test"),
            "a person is a chip: {md}"
        );
        assert!(
            md.contains(&format!("[Bridge Operations Handbook](/chat/{handbook})")),
            "a mirrored page opens its document: {md}"
        );
        assert!(
            md.contains("[Ten Forward](https://app.notion.com/p/Ten-Forward-1a2b3c4d000040008000000000001702)"),
            "a page this source does not render opens Notion: {md}"
        );
        assert!(md.contains("<time datetime=\"2369-04-15\">"), "{md}");
        assert!(md.contains(" → <time datetime=\"2369-04-16\">"), "{md}");
        assert!(
            md.contains("](https://github.test/enterprise/pull/1?a=1&b=2)"),
            "a link preview links what it previews, not its block in Notion: {md}"
        );
        assert!(
            !md.contains("<mention") && !md.contains("<link-preview"),
            "{md}"
        );
    }

    /// A link preview alone on its line looks like a layout tag, which
    /// draws nothing; it was dropped.
    #[test]
    fn a_link_preview_on_its_own_line_is_kept() {
        let md = render(
            "<link-preview url=\"https://app.notion.com/p/x#b1\" \
             source=\"https://memory-alpha.test/wiki/Neutral_Zone\"/>",
        );
        assert_eq!(
            md,
            "[https://memory-alpha.test/wiki/Neutral_Zone](https://memory-alpha.test/wiki/Neutral_Zone)\n"
        );
    }

    /// A callout is a quote led by its icon; its children are inside it.
    #[test]
    fn a_callout_is_a_quote() {
        let md = render(
            "<callout icon=\"🖖\" color=\"gray_bg\">\n\
             \tCaptain on the bridge.\n\
             \t- Yellow alert\n\
             </callout>\n\
             Dismissed.",
        );
        assert_eq!(
            md,
            "> 🖖 Captain on the bridge.\n>\n> - Yellow alert\n\nDismissed.\n"
        );
    }

    #[test]
    fn columns_and_synced_blocks_read_through() {
        let md = render(
            "<columns>\n\
             \t<column ratio=\"50\">\n\
             \t\t### Port\n\
             \t</column>\n\
             \t<column>\n\
             \t\t<synced_block url=\"https://app.notion.com/p/x\">\n\
             \t\t\tStarboard\n\
             \t\t</synced_block>\n\
             \t</column>\n\
             </columns>",
        );
        assert_eq!(md, "### Port\n\nStarboard\n");
    }

    /// A table's rows are not indented under it, so they are read as one
    /// region, the way a fence's body is.
    #[test]
    fn a_table_is_a_pipe_table() {
        let md = render(
            "- Duty roster\n\
             \t<table header-row=\"true\">\n\
             <colgroup>\n\
             <col>\n\
             </colgroup>\n\
             <tr>\n\
             <td>**Officer**</td>\n\
             <td>Post</td>\n\
             </tr>\n\
             <tr>\n\
             <td>Worf</td>\n\
             <td>Tactical | Security</td>\n\
             </tr>\n\
             \t</table>\n\
             \t- After",
        );
        assert_eq!(
            md,
            "- Duty roster\n\n  \
             | **Officer** | Post |\n  \
             | --- | --- |\n  \
             | Worf | Tactical \\| Security |\n\
             \n  \
             - After\n"
        );
    }

    #[test]
    fn a_fence_keeps_its_unindented_body() {
        let md = render("- Trace\n\t\t```text\nWarp core\n    breach\n\t\t```\n\t- Done");
        assert_eq!(
            md,
            "- Trace\n\n  ```text\n  Warp core\n      breach\n  ```\n\n  - Done\n"
        );
    }

    #[test]
    fn child_pages_files_and_videos_are_links_and_players() {
        let md = render(
            "<page url=\"https://app.notion.com/p/1a2b3c4d000040008000000000001701\">Handbook</page>\n\
             <file src=\"/data/notion/ingest/blob/abc\"></file>\n\
             <video src=\"/data/notion/ingest/blob/def\"></video>\n\
             <unknown url=\"https://app.notion.com/p/x#y\" alt=\"button\"/>\n\
             <table_of_contents color=\"gray\"/>",
        );
        let handbook =
            super::super::ids::page("notion", "1a2b3c4d-0000-4000-8000-000000001701", None).uuid;
        assert_eq!(
            md,
            format!(
                "📄 [Handbook](/chat/{handbook})\n\n\
                 📎 [Attachment](/data/notion/ingest/blob/abc)\n\n\
                 <video controls src=\"/data/notion/ingest/blob/def\"></video>\n"
            )
        );
    }

    /// A body already in CommonMark — blank lines between blocks, no
    /// tabs — reads the same afterwards.
    #[test]
    fn plain_markdown_passes_through() {
        let src = "## Audit\n\n- Plasma flow\n\n- Holodeck\n\n> There are four lights.\n\n---\n";
        assert_eq!(
            render(src),
            "## Audit\n\n- Plasma flow\n- Holodeck\n\n> There are four lights.\n\n---\n"
        );
    }
}
