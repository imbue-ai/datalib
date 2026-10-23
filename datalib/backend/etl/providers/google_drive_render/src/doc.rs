//! A Google Doc's native JSON (the Docs API's `documents.get`, every tab,
//! suggestions inline) as markdown blocks. Pure: the caller says where
//! each stored image landed, and gets back the blocks in document order
//! with the plain text of each, which is what comment threads are placed
//! by. TRANSLATE.md says what becomes what and what is left out.

use std::collections::HashMap;

use datalib_etl_render::html::{escape_md_inline, md_code_block, md_code_span, md_link_dest};
use datalib_etl_render::message::chip_link;
use datalib_handle::Handle;
use serde_json::Value;

use crate::unsupported::Unsupported;

/// One top-level piece of the body, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub md: String,
    /// What it says, as a reader would quote it: no markup, its lines
    /// joined by spaces.
    pub text: String,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Plain,
    /// A list item: the next block joins it with a single newline when
    /// it is one too, so the list stays one list.
    ListItem,
    /// A run of monospace paragraphs, fenced together.
    Code,
}

#[derive(Debug, Default)]
pub struct DocBody {
    pub blocks: Vec<Block>,
    pub unsupported: Vec<Unsupported>,
    /// Images the JSON names whose bytes the store does not hold.
    pub images_missing: Vec<String>,
}

/// Families Docs offers that are monospace. A run in one is code.
const MONOSPACE: &[&str] = &[
    "Consolas",
    "Courier New",
    "Courier Prime",
    "Cousine",
    "Fira Code",
    "IBM Plex Mono",
    "Inconsolata",
    "JetBrains Mono",
    "Noto Sans Mono",
    "Overpass Mono",
    "PT Mono",
    "Roboto Mono",
    "Source Code Pro",
    "Space Mono",
    "Ubuntu Mono",
];

const STRUCTURAL_KEYS: &[&str] = &[
    "startIndex",
    "endIndex",
    "paragraph",
    "sectionBreak",
    "table",
    "tableOfContents",
];

const PARAGRAPH_ELEMENT_KEYS: &[&str] = &[
    "startIndex",
    "endIndex",
    "textRun",
    "autoText",
    "pageBreak",
    "columnBreak",
    "footnoteReference",
    "horizontalRule",
    "equation",
    "inlineObjectElement",
    "person",
    "richLink",
    "dateElement",
    "dropdown",
];

/// `images` maps the JSON pointer an image link was taken from
/// (`take_image_links` in the download) to the path its bytes were
/// written at, relative to the document.
pub fn convert(native: &Value, images: &HashMap<String, String>) -> DocBody {
    let mut cx = Cx {
        images,
        out: DocBody::default(),
    };
    let tabs = flatten_tabs(native);
    let titled = tabs.len() > 1;
    for (pointer, tab) in tabs {
        cx.tab(&pointer, tab, titled);
    }
    cx.out
}

/// Every tab, children after their parent, with the JSON pointer of each.
fn flatten_tabs(native: &Value) -> Vec<(String, &Value)> {
    fn walk<'v>(tabs: &'v Value, at: &str, out: &mut Vec<(String, &'v Value)>) {
        for (i, tab) in tabs.as_array().into_iter().flatten().enumerate() {
            let pointer = format!("{at}/{i}");
            out.push((pointer.clone(), tab));
            if let Some(children) = tab.get("childTabs") {
                walk(children, &format!("{pointer}/childTabs"), out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(tabs) = native.get("tabs") {
        walk(tabs, "/tabs", &mut out);
    }
    out
}

struct Cx<'a> {
    images: &'a HashMap<String, String>,
    out: DocBody,
}

/// What one tab's elements need from the tab around them.
struct Tab<'v> {
    pointer: String,
    doc: &'v Value,
}

impl Tab<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.doc.get(key)
    }

    fn list_ordered(&self, list_id: &str, level: u64) -> bool {
        let glyph = self
            .get("lists")
            .and_then(|l| l.get(list_id))
            .and_then(|l| l.pointer("/listProperties/nestingLevels"))
            .and_then(|levels| levels.get(level as usize));
        let Some(glyph) = glyph else {
            return false;
        };
        let symbol = glyph.get("glyphSymbol").and_then(Value::as_str);
        let kind = glyph.get("glyphType").and_then(Value::as_str);
        symbol.is_none()
            && matches!(
                kind,
                Some(
                    "DECIMAL" | "ZERO_DECIMAL" | "ALPHA" | "UPPER_ALPHA" | "ROMAN" | "UPPER_ROMAN"
                )
            )
    }
}

impl Cx<'_> {
    fn tab(&mut self, pointer: &str, tab: &Value, titled: bool) {
        let Some(doc) = tab.get("documentTab") else {
            self.unsupported_block("tab", pointer, tab);
            return;
        };
        let tab_cx = Tab {
            pointer: format!("{pointer}/documentTab"),
            doc,
        };
        if titled {
            let title = tab
                .pointer("/tabProperties/title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled tab");
            self.out.blocks.push(Block {
                md: format!("# {}", escape_md_inline(title)),
                text: title.to_string(),
                kind: BlockKind::Plain,
            });
        }
        let body = doc
            .pointer("/body/content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let at = format!("{}/body/content", tab_cx.pointer);
        let mut code: Vec<String> = Vec::new();
        let mut footnotes: Vec<(String, String)> = Vec::new();
        for (i, el) in body.iter().enumerate() {
            let pointer = format!("{at}/{i}");
            match code_line(el) {
                Some(CodeLine::Code(line)) => {
                    code.push(line);
                    continue;
                }
                Some(CodeLine::Blank) if !code.is_empty() => {
                    code.push(String::new());
                    continue;
                }
                _ => {}
            }
            self.flush_code(&mut code);
            self.structural(&tab_cx, &pointer, el, &mut footnotes);
        }
        self.flush_code(&mut code);
        self.footnotes(&tab_cx, &footnotes);
    }

    fn flush_code(&mut self, code: &mut Vec<String>) {
        while code.last().is_some_and(String::is_empty) {
            code.pop();
        }
        if code.is_empty() {
            return;
        }
        let body = code.join("\n");
        self.out.blocks.push(Block {
            md: md_code_block("", &body),
            text: code.join(" "),
            kind: BlockKind::Code,
        });
        code.clear();
    }

    fn structural(
        &mut self,
        tab: &Tab,
        pointer: &str,
        el: &Value,
        footnotes: &mut Vec<(String, String)>,
    ) {
        if let Some(unknown) = unknown_key(el, STRUCTURAL_KEYS) {
            self.unsupported_block(unknown, pointer, el);
            return;
        }
        if let Some(p) = el.get("paragraph") {
            let pointer = format!("{pointer}/paragraph");
            if let Some(block) = self.paragraph(tab, &pointer, p, footnotes) {
                self.out.blocks.push(block);
            }
            self.positioned(tab, p);
        } else if let Some(t) = el.get("table") {
            self.table(tab, &format!("{pointer}/table"), t, footnotes);
        }
        // A section break is page layout, and a table of contents is the
        // headings again, which are already here.
    }

    /// One paragraph: a heading, a list item or a plain paragraph. `None`
    /// for one with nothing in it.
    fn paragraph(
        &mut self,
        tab: &Tab,
        pointer: &str,
        p: &Value,
        footnotes: &mut Vec<(String, String)>,
    ) -> Option<Block> {
        let (md, text) = self.inline(tab, pointer, p, footnotes);
        if md.trim().is_empty() {
            return None;
        }
        let style = p
            .pointer("/paragraphStyle/namedStyleType")
            .and_then(Value::as_str);
        let heading = match style {
            Some("TITLE") | Some("HEADING_1") => Some(1),
            Some("HEADING_2") => Some(2),
            Some("HEADING_3") => Some(3),
            Some("HEADING_4") => Some(4),
            Some("HEADING_5") => Some(5),
            Some("HEADING_6") => Some(6),
            _ => None,
        };
        if let Some(level) = heading {
            return Some(Block {
                md: format!("{} {}", "#".repeat(level), md.replace('\n', " ")),
                text,
                kind: BlockKind::Plain,
            });
        }
        if let Some(bullet) = p.get("bullet") {
            let level = bullet
                .get("nestingLevel")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let ordered = bullet
                .get("listId")
                .and_then(Value::as_str)
                .is_some_and(|id| tab.list_ordered(id, level));
            let marker = if ordered { "1." } else { "-" };
            let indent = " ".repeat(4 * level as usize);
            let hang = " ".repeat(indent.len() + marker.len() + 1);
            return Some(Block {
                md: format!(
                    "{indent}{marker} {}",
                    md.replace('\n', &format!("\n{hang}"))
                ),
                text,
                kind: BlockKind::ListItem,
            });
        }
        let md = if style == Some("SUBTITLE") {
            format!("*{}*", md.trim())
        } else {
            md
        };
        Some(Block {
            md,
            text,
            kind: BlockKind::Plain,
        })
    }

    /// A paragraph's elements as one line of markdown, and its text.
    fn inline(
        &mut self,
        tab: &Tab,
        pointer: &str,
        p: &Value,
        footnotes: &mut Vec<(String, String)>,
    ) -> (String, String) {
        let mut runs: Vec<Run> = Vec::new();
        let mut trailing: Vec<Unsupported> = Vec::new();
        let elements = p
            .get("elements")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (i, e) in elements.iter().enumerate() {
            let pointer = format!("{pointer}/elements/{i}");
            if let Some(unknown) = unknown_key(e, PARAGRAPH_ELEMENT_KEYS) {
                runs.push(Run::markup(
                    md_code_span(&format!("unsupported: {unknown}")),
                    String::new(),
                ));
                trailing.push(Unsupported::new(unknown, &pointer, e));
                continue;
            }
            if let Some(t) = e.get("textRun") {
                let content = t.get("content").and_then(Value::as_str).unwrap_or("");
                runs.push(Run::text(content, Style::of(t)));
            } else if let Some(person) = e.get("person") {
                let props = person.get("personProperties");
                let name = props.and_then(|p| p.get("name")).and_then(Value::as_str);
                let email = props.and_then(|p| p.get("email")).and_then(Value::as_str);
                let shown = name.or(email).unwrap_or("");
                let md = match email.and_then(Handle::email) {
                    Some(h) => chip_link(shown, &h),
                    None => escape_md_inline(shown),
                };
                runs.push(Run::markup(md, shown.to_string()));
            } else if let Some(link) = e.get("richLink") {
                let props = link.get("richLinkProperties");
                let uri = props.and_then(|p| p.get("uri")).and_then(Value::as_str);
                let title = props
                    .and_then(|p| p.get("title"))
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .or(uri)
                    .unwrap_or("");
                let md = match uri {
                    Some(uri) => format!("[{}]({})", escape_md_inline(title), md_link_dest(uri)),
                    None => escape_md_inline(title),
                };
                runs.push(Run::markup(md, title.to_string()));
            } else if let Some(date) = e.get("dateElement") {
                let shown = date
                    .pointer("/dateElementProperties/displayText")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                runs.push(Run::text(shown, Style::default()));
            } else if let Some(dropdown) = e.get("dropdown") {
                let shown = dropdown
                    .pointer("/dropdownProperties/displayValue")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                runs.push(Run::text(shown, Style::default()));
            } else if let Some(fref) = e.get("footnoteReference") {
                let n = fref
                    .get("footnoteNumber")
                    .and_then(Value::as_str)
                    .unwrap_or("*");
                if let Some(id) = fref.get("footnoteId").and_then(Value::as_str) {
                    footnotes.push((id.to_string(), n.to_string()));
                }
                runs.push(Run::markup(
                    format!("<sup>{}</sup>", escape_md_inline(n)),
                    String::new(),
                ));
            } else if let Some(obj) = e.get("inlineObjectElement") {
                let id = obj
                    .get("inlineObjectId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let md = self.image(tab, "inlineObjects", "inlineObjectProperties", id);
                runs.push(Run::markup(md, String::new()));
            } else if e.get("horizontalRule").is_some() {
                runs.push(Run::markup("\n\n---\n\n".to_string(), String::new()));
            } else if e.get("equation").is_some() {
                // The API carries an equation's place and nothing of what
                // it says.
                runs.push(Run::markup("*(equation)*".to_string(), String::new()));
            }
            // An auto text (a page number), a page or column break: page
            // furniture, with nothing to say in a document that has no pages.
        }
        let (md, text) = render_runs(&runs);
        let mut md = md;
        for u in trailing {
            md.push_str("\n\n");
            md.push_str(&u.details());
            self.out.unsupported.push(u);
        }
        (md, text)
    }

    /// An image object (`inlineObjects` or `positionedObjects`) by id.
    fn image(&mut self, tab: &Tab, map: &str, props: &str, id: &str) -> String {
        let Some(obj) = tab.get(map).and_then(|m| m.get(id)) else {
            return "*(image not in the document)*".to_string();
        };
        let embedded = obj.get(props).and_then(|p| p.get("embeddedObject"));
        let Some(embedded) = embedded.filter(|e| e.get("imageProperties").is_some()) else {
            let u = Unsupported::new(
                "embeddedObject",
                &format!("{}/{map}/{}", tab.pointer, escape_pointer(id)),
                obj,
            );
            let md = format!("\n\n{}\n\n", u.details());
            self.out.unsupported.push(u);
            return md;
        };
        let alt = ["title", "description"]
            .iter()
            .find_map(|k| embedded.get(*k).and_then(Value::as_str))
            .unwrap_or("image");
        let pointer = format!(
            "{}/{map}/{}/{props}/embeddedObject/imageProperties/contentUri",
            tab.pointer,
            escape_pointer(id)
        );
        match self.images.get(&pointer) {
            Some(path) => format!("![{}]({})", escape_md_inline(alt), md_link_dest(path)),
            None => {
                self.out.images_missing.push(pointer);
                format!("*(image not downloaded: {})*", escape_md_inline(alt))
            }
        }
    }

    /// Images floated beside a paragraph, after it.
    fn positioned(&mut self, tab: &Tab, p: &Value) {
        for id in p
            .get("positionedObjectIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let md = self.image(tab, "positionedObjects", "positionedObjectProperties", id);
            self.out.blocks.push(Block {
                md,
                text: String::new(),
                kind: BlockKind::Plain,
            });
        }
    }

    /// A table as a markdown table, its first row the header. A cell's
    /// paragraphs are joined with `<br>`, since a table row is one line.
    fn table(
        &mut self,
        tab: &Tab,
        pointer: &str,
        t: &Value,
        footnotes: &mut Vec<(String, String)>,
    ) {
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        for (r, row) in t
            .get("tableRows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut cells = Vec::new();
            for (c, cell) in row
                .get("tableCells")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let at = format!("{pointer}/tableRows/{r}/tableCells/{c}/content");
                cells.push(self.cell(tab, &at, cell, footnotes));
            }
            rows.push(cells);
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            return;
        }
        let line = |cells: &[(String, String)]| {
            let mut s = String::from("|");
            for i in 0..width {
                let md = cells.get(i).map(|(md, _)| md.as_str()).unwrap_or("");
                s.push(' ');
                s.push_str(md);
                s.push_str(" |");
            }
            s
        };
        let mut md = line(&rows[0]);
        md.push_str(&format!("\n|{}", " --- |".repeat(width)));
        for row in &rows[1..] {
            md.push('\n');
            md.push_str(&line(row));
        }
        let text = rows
            .iter()
            .flatten()
            .map(|(_, text)| text.as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.out.blocks.push(Block {
            md,
            text,
            kind: BlockKind::Plain,
        });
    }

    fn cell(
        &mut self,
        tab: &Tab,
        pointer: &str,
        cell: &Value,
        footnotes: &mut Vec<(String, String)>,
    ) -> (String, String) {
        let mut parts: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for (i, el) in cell
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let at = format!("{pointer}/{i}");
            if let Some(unknown) = unknown_key(el, STRUCTURAL_KEYS) {
                parts.push(md_code_span(&format!("unsupported: {unknown}")));
                self.out
                    .unsupported
                    .push(Unsupported::new(unknown, &at, el));
                continue;
            }
            if let Some(p) = el.get("paragraph") {
                let (md, text) = self.inline(tab, &format!("{at}/paragraph"), p, footnotes);
                if md.trim().is_empty() {
                    continue;
                }
                let bullet = if p.get("bullet").is_some() {
                    "• "
                } else {
                    ""
                };
                parts.push(format!("{bullet}{}", md.trim()));
                texts.push(text);
            } else if el.get("table").is_some() {
                // A table inside a table cell has no markdown form; its
                // text is kept on one line.
                let mut inner = Vec::new();
                collect_text(el, &mut inner);
                let text = inner.join(" ");
                parts.push(escape_md_inline(&text));
                texts.push(text);
            }
        }
        let md = parts
            .join("<br>")
            .replace("\n\n", "<br>")
            .replace('\n', " ");
        (md, texts.join(" "))
    }

    fn footnotes(&mut self, tab: &Tab, refs: &[(String, String)]) {
        for (id, n) in refs {
            let Some(content) = tab
                .get("footnotes")
                .and_then(|f| f.get(id))
                .and_then(|f| f.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            let at = format!("{}/footnotes/{}/content", tab.pointer, escape_pointer(id));
            let mut mds = Vec::new();
            let mut texts = Vec::new();
            let mut nested = Vec::new();
            for (i, el) in content.iter().enumerate() {
                if let Some(p) = el.get("paragraph") {
                    let (md, text) =
                        self.inline(tab, &format!("{at}/{i}/paragraph"), p, &mut nested);
                    if !md.trim().is_empty() {
                        mds.push(md.trim().to_string());
                        texts.push(text);
                    }
                }
            }
            self.out.blocks.push(Block {
                md: format!("<sup>{}</sup> {}", escape_md_inline(n), mds.join(" ")),
                text: texts.join(" "),
                kind: BlockKind::Plain,
            });
        }
    }

    fn unsupported_block(&mut self, key: &str, pointer: &str, value: &Value) {
        let u = Unsupported::new(key, pointer, value);
        self.out.blocks.push(Block {
            md: u.details(),
            text: String::new(),
            kind: BlockKind::Plain,
        });
        self.out.unsupported.push(u);
    }
}

/// The first key of `v` that is none of `known`.
fn unknown_key<'v>(v: &'v Value, known: &[&str]) -> Option<&'v str> {
    v.as_object()?
        .keys()
        .map(String::as_str)
        .find(|k| !known.contains(k))
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn collect_text(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("content") {
                let s = s.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
            for (k, child) in map {
                if k != "content" || !child.is_string() {
                    collect_text(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|c| collect_text(c, out)),
        _ => {}
    }
}

/// Docs' own marker around a code block, in the text where the block
/// starts and ends; nothing a reader sees.
const CODE_BLOCK_MARK: char = '\u{e907}';

enum CodeLine {
    Code(String),
    /// A paragraph with nothing in it, which a code block keeps as an
    /// empty line.
    Blank,
}

/// The text of a paragraph set wholly in a monospace font, which reads
/// as a line of code; `None` for anything else.
fn code_line(el: &Value) -> Option<CodeLine> {
    let p = el.get("paragraph")?;
    if p.get("bullet").is_some() {
        return None;
    }
    let style = p
        .pointer("/paragraphStyle/namedStyleType")
        .and_then(Value::as_str);
    if style.is_some_and(|s| s != "NORMAL_TEXT") {
        return None;
    }
    let elements = p.get("elements")?.as_array()?;
    let mut line = String::new();
    let mut any_code = false;
    for e in elements {
        let t = e.get("textRun")?;
        let content = t.get("content").and_then(Value::as_str).unwrap_or("");
        let content = content.replace(CODE_BLOCK_MARK, "");
        if content.trim().is_empty() {
            line.push_str(&content);
            continue;
        }
        if !Style::of(t).code || t.get("suggestedDeletionIds").is_some() {
            return None;
        }
        any_code = true;
        line.push_str(&content);
    }
    if !any_code {
        return Some(CodeLine::Blank);
    }
    Some(CodeLine::Code(
        line.trim_end_matches('\n').replace('\u{b}', "\n"),
    ))
}

/// How a run of text is set, as far as markdown can say it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    sup: bool,
    sub: bool,
    link: Option<String>,
    inserted: bool,
    deleted: bool,
}

impl Style {
    fn of(run: &Value) -> Self {
        let ts = run.get("textStyle");
        let flag = |k: &str| {
            ts.and_then(|s| s.get(k))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let family = ts
            .and_then(|s| s.pointer("/weightedFontFamily/fontFamily"))
            .and_then(Value::as_str);
        let offset = ts
            .and_then(|s| s.get("baselineOffset"))
            .and_then(Value::as_str);
        let has = |k: &str| {
            run.get(k)
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
        };
        Style {
            bold: flag("bold"),
            italic: flag("italic"),
            strike: flag("strikethrough"),
            code: family.is_some_and(|f| MONOSPACE.contains(&f)),
            sup: offset == Some("SUPERSCRIPT"),
            sub: offset == Some("SUBSCRIPT"),
            link: ts
                .and_then(|s| s.pointer("/link/url"))
                .and_then(Value::as_str)
                .map(str::to_string),
            inserted: has("suggestedInsertionIds"),
            deleted: has("suggestedDeletionIds"),
        }
    }
}

/// A piece of a paragraph: plain text to escape and style, or markup a
/// chip or an image already made.
#[derive(Debug, Clone)]
enum Run {
    Text(String, Style),
    Markup { md: String, text: String },
}

impl Run {
    fn text(s: &str, style: Style) -> Self {
        Run::Text(s.to_string(), style)
    }

    fn markup(md: String, text: String) -> Self {
        Run::Markup { md, text }
    }
}

/// The runs of one paragraph as markdown and as plain text. Neighbouring
/// runs set alike are merged first, so a word Docs split in two is not
/// `**a****b**`.
fn render_runs(runs: &[Run]) -> (String, String) {
    let mut merged: Vec<Run> = Vec::new();
    for run in runs {
        match (merged.last_mut(), run) {
            (Some(Run::Text(prev, a)), Run::Text(s, b)) if a == b => prev.push_str(s),
            _ => merged.push(run.clone()),
        }
    }
    let mut md = String::new();
    let mut text = String::new();
    for run in &merged {
        match run {
            Run::Text(s, style) => {
                let s = s
                    .replace(['\n', CODE_BLOCK_MARK], "")
                    .replace('\u{b}', "\n");
                text.push_str(&s);
                md.push_str(&styled(&s, style));
            }
            Run::Markup { md: m, text: t } => {
                md.push_str(m);
                text.push_str(t);
            }
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (md.trim_end().to_string(), text)
}

/// One run with its style as markdown. Whitespace at either end stays
/// outside the markers, which markdown does not close across.
fn styled(s: &str, style: &Style) -> String {
    let lines: Vec<String> = s.split('\n').map(|line| styled_line(line, style)).collect();
    lines.join("<br>\n")
}

fn styled_line(s: &str, style: &Style) -> String {
    let core = s.trim();
    if core.is_empty() {
        return s.to_string();
    }
    let lead = &s[..s.len() - s.trim_start().len()];
    let trail = &s[s.trim_end().len()..];
    let mut out = if style.code {
        md_code_span(core)
    } else {
        escape_md_inline(core)
    };
    if !style.code {
        for (on, marker) in [
            (style.strike, "~~"),
            (style.italic, "*"),
            (style.bold, "**"),
        ] {
            if on {
                out = format!("{marker}{out}{marker}");
            }
        }
    }
    if style.sup {
        out = format!("<sup>{out}</sup>");
    } else if style.sub {
        out = format!("<sub>{out}</sub>");
    }
    if let Some(url) = &style.link {
        out = format!("[{out}]({})", md_link_dest(url));
    }
    if style.deleted {
        out = format!("<del>{out}</del>");
    } else if style.inserted {
        out = format!("<ins>{out}</ins>");
    }
    format!("{lead}{out}{trail}")
}

/// The blocks as one markdown body: a blank line between blocks, one
/// newline between two items of the same list.
pub fn join_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            let same_list =
                b.kind == BlockKind::ListItem && blocks[i - 1].kind == BlockKind::ListItem;
            out.push_str(if same_list { "\n" } else { "\n\n" });
        }
        out.push_str(&b.md);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(content: Value) -> Value {
        json!({"tabs": [{"tabProperties": {"tabId": "t.0", "title": "Tab 1"},
                         "documentTab": {"body": {"content": content},
                                          "lists": {"bullets": {"listProperties": {"nestingLevels": [
                                              {"glyphSymbol": "●"}, {"glyphSymbol": "○"}]}},
                                                    "numbers": {"listProperties": {"nestingLevels": [
                                              {"glyphType": "DECIMAL"}]}}}}}]})
    }

    fn para(elements: Value) -> Value {
        json!({"paragraph": {"elements": elements, "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"}}})
    }

    fn styled_para(style: &str, text: &str) -> Value {
        json!({"paragraph": {"elements": [{"textRun": {"content": text}}],
                             "paragraphStyle": {"namedStyleType": style}}})
    }

    fn run(text: &str, style: Value) -> Value {
        json!({"textRun": {"content": text, "textStyle": style}})
    }

    fn render(content: Value) -> String {
        join_blocks(&convert(&doc(content), &HashMap::new()).blocks)
    }

    #[test]
    fn headings_keep_their_level_and_a_title_is_the_top_one() {
        let md = render(json!([
            styled_para("TITLE", "Captain's log\n"),
            styled_para("HEADING_2", "Stardate 41153.7\n"),
            styled_para("NORMAL_TEXT", "We are departing Farpoint.\n"),
        ]));
        assert_eq!(
            md,
            "# Captain's log\n\n## Stardate 41153.7\n\nWe are departing Farpoint."
        );
    }

    #[test]
    fn emphasis_and_links_survive_and_split_runs_merge() {
        let md = render(json!([para(json!([
            run("Engage ", json!({})),
            run("warp ", json!({"bold": true})),
            run("nine", json!({"bold": true})),
            run(" and see ", json!({})),
            run(
                "the manual",
                json!({"link": {"url": "https://example.test/manual (v2)"}})
            ),
            run(".\n", json!({})),
        ]))]));
        assert_eq!(
            md,
            "Engage **warp nine** and see [the manual](<https://example.test/manual (v2)>)."
        );
    }

    #[test]
    fn lists_nest_and_number_and_stay_one_list() {
        let item = |list: &str, level: u64, text: &str| {
            json!({"paragraph": {"elements": [{"textRun": {"content": text}}],
                                 "bullet": {"listId": list, "nestingLevel": level},
                                 "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"}}})
        };
        let md = render(json!([
            item("bullets", 0, "Bridge\n"),
            item("bullets", 1, "Ops\n"),
            item("numbers", 0, "Engage\n"),
            styled_para("NORMAL_TEXT", "After.\n"),
        ]));
        assert_eq!(md, "- Bridge\n    - Ops\n1. Engage\n\nAfter.");
    }

    #[test]
    fn a_monospace_run_is_code_and_monospace_paragraphs_are_one_block() {
        let mono = json!({"weightedFontFamily": {"fontFamily": "Courier New"}});
        let md = render(json!([
            para(json!([
                run("Run ", json!({})),
                run("make it so", mono.clone()),
                run("\n", json!({}))
            ])),
            para(json!([run("fn engage() {\n", mono.clone())])),
            para(json!([run("}\n", mono)])),
        ]));
        assert_eq!(md, "Run `make it so`\n\n```\nfn engage() {\n}\n```");
    }

    /// Docs' code block building block marks its start with a character
    /// of its own, in no font, and a blank line inside it is a paragraph
    /// with nothing in it; neither may cut the code in two.
    #[test]
    fn a_docs_code_block_is_one_fence_without_its_marker() {
        let mono = json!({"weightedFontFamily": {"fontFamily": "Roboto Mono"}});
        let md = render(json!([
            para(json!([
                run("\u{e907}", json!({})),
                run("def engage():\n", mono.clone())
            ])),
            para(json!([run("\n", json!({}))])),
            para(json!([run("    return warp(9)\n", mono)])),
            para(json!([run("\u{e907}\n", json!({}))])),
            styled_para("NORMAL_TEXT", "After.\n"),
        ]));
        assert_eq!(
            md,
            "```\ndef engage():\n\n    return warp(9)\n```\n\nAfter."
        );
    }

    #[test]
    fn a_person_chip_is_a_chip_link_and_a_rich_link_a_link() {
        let md = render(json!([para(json!([
            {"person": {"personProperties": {"name": "Will Riker", "email": "riker@enterprise.test"}}},
            run(" read ", json!({})),
            {"richLink": {"richLinkProperties": {"title": "Away team roster",
                "uri": "https://docs.google.com/document/d/1Roster/edit"}}},
            run(" on ", json!({})),
            {"dateElement": {"dateElementProperties": {"displayText": "Oct 21, 2364"}}},
        ]))]));
        assert_eq!(
            md,
            "[Will Riker](mailto:riker@enterprise.test \"Will Riker <riker@enterprise.test>\") \
             read [Away team roster](https://docs.google.com/document/d/1Roster/edit) on Oct 21, 2364"
        );
    }

    #[test]
    fn a_suggestion_shows_as_inserted_or_deleted_text() {
        let md = render(json!([para(json!([
            {"textRun": {"content": "Set course for ", "textStyle": {}}},
            {"textRun": {"content": "Risa", "textStyle": {}, "suggestedDeletionIds": ["suggest.a"]}},
            {"textRun": {"content": "Vulcan", "textStyle": {}, "suggestedInsertionIds": ["suggest.b"]}},
            {"textRun": {"content": "\n", "textStyle": {}}},
        ]))]));
        assert_eq!(md, "Set course for <del>Risa</del><ins>Vulcan</ins>");
    }

    #[test]
    fn a_table_is_a_markdown_table_with_its_cells_on_one_line() {
        let cell = |text: &str| json!({"content": [styled_para("NORMAL_TEXT", text)]});
        let md = render(json!([{"table": {"rows": 2, "columns": 2, "tableRows": [
            {"tableCells": [cell("Officer\n"), cell("Post\n")]},
            {"tableCells": [cell("Data\n"), {"content": [
                styled_para("NORMAL_TEXT", "Ops\n"), styled_para("NORMAL_TEXT", "a | b\n")]}]},
        ]}}]));
        assert_eq!(
            md,
            "| Officer | Post |\n| --- | --- |\n| Data | Ops<br>a \\| b |"
        );
    }

    #[test]
    fn an_image_is_the_file_its_bytes_were_written_to() {
        let mut d = doc(json!([para(json!([
            {"inlineObjectElement": {"inlineObjectId": "kix.enterprise"}},
            run("\n", json!({})),
        ]))]));
        d["tabs"][0]["documentTab"]["inlineObjects"] = json!({"kix.enterprise": {
            "inlineObjectProperties": {"embeddedObject": {"title": "NCC-1701-D",
                "imageProperties": {}}}}});
        let pointer = "/tabs/0/documentTab/inlineObjects/kix.enterprise/inlineObjectProperties/\
                       embeddedObject/imageProperties/contentUri";
        let images = HashMap::from([(pointer.to_string(), "blobs/0a1b.png".to_string())]);
        let body = convert(&d, &images);
        assert_eq!(join_blocks(&body.blocks), "![NCC-1701-D](blobs/0a1b.png)");
        let body = convert(&d, &HashMap::new());
        assert_eq!(
            join_blocks(&body.blocks),
            "*(image not downloaded: NCC-1701-D)*"
        );
        assert_eq!(body.images_missing, vec![pointer.to_string()]);
    }

    #[test]
    fn footnotes_follow_their_tab() {
        let mut d = doc(json!([para(json!([
            run("Warp nine", json!({})),
            {"footnoteReference": {"footnoteId": "kix.fn1", "footnoteNumber": "1"}},
            run("\n", json!({})),
        ]))]));
        d["tabs"][0]["documentTab"]["footnotes"] = json!({"kix.fn1": {"content": [
            styled_para("NORMAL_TEXT", "Briefly.\n")]}});
        assert_eq!(
            join_blocks(&convert(&d, &HashMap::new()).blocks),
            "Warp nine<sup>1</sup>\n\n<sup>1</sup> Briefly."
        );
    }

    #[test]
    fn several_tabs_each_get_a_heading() {
        let d = json!({"tabs": [
            {"tabProperties": {"title": "Log"}, "documentTab": {"body": {"content": [
                styled_para("NORMAL_TEXT", "One.\n")]}}},
            {"tabProperties": {"title": "Notes"}, "documentTab": {"body": {"content": [
                styled_para("NORMAL_TEXT", "Two.\n")]}},
             "childTabs": [{"tabProperties": {"title": "Aside"}, "documentTab": {"body": {"content": [
                styled_para("NORMAL_TEXT", "Three.\n")]}}}]},
        ]});
        assert_eq!(
            join_blocks(&convert(&d, &HashMap::new()).blocks),
            "# Log\n\nOne.\n\n# Notes\n\nTwo.\n\n# Aside\n\nThree."
        );
    }

    /// Something the renderer does not know is shown, not dropped, and
    /// reported where it was.
    #[test]
    fn an_unknown_element_is_kept_as_json_and_reported() {
        let body = convert(
            &doc(json!([
                para(json!([run("Before ", json!({})), {"holodeckProgram": {"name": "Dixon Hill"}}])),
                {"startIndex": 9, "subspaceField": {"strength": 9}},
            ])),
            &HashMap::new(),
        );
        let md = join_blocks(&body.blocks);
        assert!(
            md.starts_with("Before `unsupported: holodeckProgram`\n\n<details"),
            "{md}"
        );
        assert!(md.contains("\"name\": \"Dixon Hill\""), "{md}");
        assert!(md.contains("\"strength\": 9"), "{md}");
        let found: Vec<(&str, &str)> = body
            .unsupported
            .iter()
            .map(|u| (u.key.as_str(), u.pointer.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "holodeckProgram",
                    "/tabs/0/documentTab/body/content/0/paragraph/elements/1"
                ),
                ("subspaceField", "/tabs/0/documentTab/body/content/1"),
            ]
        );
    }

    #[test]
    fn text_in_markup_reads_as_typed() {
        let md = render(json!([
            styled_para("NORMAL_TEXT", "<script>x</script> & co\n"),
            styled_para("NORMAL_TEXT", "# not a heading\n"),
            styled_para("HEADING_1", "[a](https://e.test)\n"),
        ]));
        assert_eq!(
            md,
            "&lt;script&gt;x&lt;/script&gt; &amp; co\n\n\\# not a heading\n\n# \\[a\\](https://e.test)"
        );
    }

    #[test]
    fn a_soft_line_break_is_a_line_break() {
        let md = render(json!([styled_para("NORMAL_TEXT", "Picard\u{b}Riker\n")]));
        assert_eq!(md, "Picard<br>\nRiker");
    }
}
