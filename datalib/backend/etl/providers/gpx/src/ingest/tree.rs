//! A GPX file as a small XML tree that keeps what writing it back needs:
//! names, attribute values and text exactly as written, and the
//! whitespace between elements as a per-path [`Layout`] rather than as
//! nodes. Comments are dropped. Nothing here knows GPX.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    /// Character data as written: references stay unexpanded.
    Text(String),
    /// What sits between `<![CDATA[` and `]]>`.
    CData(String),
    /// A processing instruction, `<?` and `?>` included.
    Pi(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub name: String,
    /// Names and values as written: references stay unexpanded.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    /// Written `<x/>` rather than `<x></x>`.
    pub self_closing: bool,
}

impl Element {
    /// An element whose only text is the whitespace between its child
    /// elements. That whitespace is layout, so it lives in [`Layout`]
    /// and not among the children.
    pub fn is_element_only(&self) -> bool {
        self.children.iter().any(|c| matches!(c, Node::Element(_)))
            && self
                .children
                .iter()
                .all(|c| !matches!(c, Node::Text(_) | Node::CData(_)))
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The whitespace a writer put between elements, by element path
/// (`gpx/trk/trkseg/trkpt`). One string per path is all a consistent
/// writer needs; a file that varies within a path is written back with
/// the first one seen, and the round-trip check reports it. Depth alone
/// is not enough: a writer may put a track's `<extensions>` on one line
/// and its points one per line, at the same depth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// Before an element's start tag, by its path.
    pub open: BTreeMap<String, String>,
    /// Before an element's end tag, by its path.
    pub close: BTreeMap<String, String>,
    /// How a self-closing tag ends: `/>` or ` />`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_close: Option<String>,
}

impl Layout {
    fn at<'a>(slots: &'a BTreeMap<String, String>, path: &str) -> &'a str {
        slots.get(path).map(String::as_str).unwrap_or("")
    }

    fn record(slots: &mut BTreeMap<String, String>, path: &str, ws: &str) {
        slots
            .entry(path.to_string())
            .or_insert_with(|| ws.to_string());
    }
}

pub fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// A parsed document: everything up to and including the root's start
/// tag and everything from its end tag on are kept as written, since
/// they are small and full of things (the XML declaration, namespace
/// declarations, attribute line breaks) a writer would otherwise have to
/// remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub prolog: String,
    pub root: Element,
    pub epilog: String,
    pub layout: Layout,
}

pub fn parse(src: &str) -> Result<Doc> {
    let mut reader = Reader::from_str(src);
    let mut stack: Vec<Element> = Vec::new();
    let mut prolog: Option<String> = None;
    let mut self_close: Option<String> = None;
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .with_context(|| format!("XML error near byte {}", reader.error_position()))?;
        let after = reader.buffer_position() as usize;
        let raw = &src[before..after];
        match event {
            Event::Start(e) => {
                let el = start_element(&e, false)?;
                if stack.is_empty() {
                    if prolog.is_some() {
                        bail!("a second root element at byte {before}");
                    }
                    prolog = Some(src[..after].to_string());
                }
                stack.push(el);
            }
            Event::Empty(e) => {
                let el = start_element(&e, true)?;
                if self_close.is_none() {
                    self_close = Some(self_close_style(raw));
                }
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Node::Element(el)),
                    None => {
                        return Ok(Doc {
                            prolog: src[..after].to_string(),
                            root: el,
                            epilog: src[after..].to_string(),
                            layout: Layout {
                                self_close,
                                ..Layout::default()
                            },
                        });
                    }
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or_else(|| anyhow!("unmatched end tag"))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Node::Element(el)),
                    None => {
                        let mut root = el;
                        let mut layout = Layout {
                            self_close,
                            ..Layout::default()
                        };
                        let path = root.name.clone();
                        normalize(&mut root, &path, &mut layout);
                        return Ok(Doc {
                            prolog: prolog.unwrap_or_default(),
                            root,
                            epilog: src[before..].to_string(),
                            layout,
                        });
                    }
                }
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                if let Some(el) = stack.last_mut() {
                    push_text(el, raw);
                }
            }
            Event::CData(e) => {
                if let Some(el) = stack.last_mut() {
                    el.children
                        .push(Node::CData(String::from_utf8(e.to_vec())?));
                }
            }
            Event::PI(_) => {
                if let Some(el) = stack.last_mut() {
                    el.children.push(Node::Pi(raw.to_string()));
                }
            }
            Event::Comment(_) | Event::Decl(_) | Event::DocType(_) => {}
            Event::Eof => bail!("the document ended before its root element closed"),
        }
    }
}

/// Parse a run of nodes written by [`write_compact`], exactly as they
/// were: no whitespace is taken for layout, because a compact writer
/// left none that was not content.
pub fn parse_fragment(xml: &str) -> Result<Vec<Node>> {
    let wrapped = format!("<f>{xml}</f>");
    let mut reader = Reader::from_str(&wrapped);
    let mut stack: Vec<Element> = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().context("stored XML fragment")?;
        let after = reader.buffer_position() as usize;
        let raw = &wrapped[before..after];
        match event {
            Event::Start(e) => stack.push(start_element(&e, false)?),
            Event::Empty(e) => {
                let el = start_element(&e, true)?;
                stack
                    .last_mut()
                    .ok_or_else(|| anyhow!("fragment outside its wrapper"))?
                    .children
                    .push(Node::Element(el));
            }
            Event::End(_) => {
                let el = stack.pop().ok_or_else(|| anyhow!("unmatched end tag"))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Node::Element(el)),
                    None => return Ok(el.children),
                }
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                if let Some(el) = stack.last_mut() {
                    push_text(el, raw);
                }
            }
            Event::CData(e) => {
                if let Some(el) = stack.last_mut() {
                    el.children
                        .push(Node::CData(String::from_utf8(e.to_vec())?));
                }
            }
            Event::PI(_) => {
                if let Some(el) = stack.last_mut() {
                    el.children.push(Node::Pi(raw.to_string()));
                }
            }
            Event::Comment(_) | Event::Decl(_) | Event::DocType(_) => {}
            Event::Eof => bail!("stored XML fragment is truncated"),
        }
    }
}

/// The attributes [`write_attrs`] wrote, read back.
pub fn parse_attrs(xml: &str) -> Result<Vec<(String, String)>> {
    let wrapped = format!("<a{xml}/>");
    let mut reader = Reader::from_str(&wrapped);
    match reader.read_event().context("stored attributes")? {
        Event::Empty(e) => Ok(start_element(&e, true)?.attrs),
        other => bail!("stored attributes did not parse as a tag: {other:?}"),
    }
}

fn start_element(e: &quick_xml::events::BytesStart<'_>, self_closing: bool) -> Result<Element> {
    let name = String::from_utf8(e.name().as_ref().to_vec())?;
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.with_context(|| format!("attribute of <{name}>"))?;
        attrs.push((
            String::from_utf8(a.key.as_ref().to_vec())?,
            String::from_utf8(a.value.to_vec())?,
        ));
    }
    Ok(Element {
        name,
        attrs,
        children: Vec::new(),
        self_closing,
    })
}

/// Adjacent text and references are one run; a comment between two runs
/// is dropped, so they join too.
fn push_text(el: &mut Element, raw: &str) {
    if let Some(Node::Text(t)) = el.children.last_mut() {
        t.push_str(raw);
    } else {
        el.children.push(Node::Text(raw.to_string()));
    }
}

fn self_close_style(raw: &str) -> String {
    let body = raw.strip_suffix("/>").unwrap_or(raw);
    let ws = body.len() - body.trim_end_matches(is_xml_ws).len();
    format!("{}/>", &body[body.len() - ws..])
}

fn is_xml_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

pub fn is_whitespace(s: &str) -> bool {
    s.chars().all(is_xml_ws)
}

fn normalize(el: &mut Element, path: &str, layout: &mut Layout) {
    let element_only = el.children.iter().any(|c| matches!(c, Node::Element(_)))
        && el.children.iter().all(|c| match c {
            Node::Text(t) => is_whitespace(t),
            Node::CData(_) => false,
            _ => true,
        });
    if !element_only {
        for c in &mut el.children {
            if let Node::Element(e) = c {
                let p = child_path(path, &e.name);
                normalize(e, &p, layout);
            }
        }
        return;
    }
    let mut ws = String::new();
    let mut kept = Vec::with_capacity(el.children.len());
    for c in el.children.drain(..) {
        match c {
            Node::Text(t) => ws.push_str(&t),
            Node::Element(mut e) => {
                let p = child_path(path, &e.name);
                Layout::record(&mut layout.open, &p, &ws);
                ws.clear();
                normalize(&mut e, &p, layout);
                kept.push(Node::Element(e));
            }
            other => {
                ws.clear();
                kept.push(other);
            }
        }
    }
    Layout::record(&mut layout.close, path, &ws);
    el.children = kept;
}

pub fn write_attrs(out: &mut String, attrs: &[(String, String)]) {
    for (k, v) in attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(v);
        out.push('"');
    }
}

/// `path` is the element's own, from the root: [`Layout`] is keyed by it.
pub fn write_element(out: &mut String, el: &Element, path: &str, layout: &Layout) {
    out.push('<');
    out.push_str(&el.name);
    write_attrs(out, &el.attrs);
    if el.self_closing && el.children.is_empty() {
        out.push_str(layout.self_close.as_deref().unwrap_or("/>"));
        return;
    }
    out.push('>');
    write_body(out, el, path, layout);
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}

/// An element's children and the whitespace before its end tag: what
/// sits between its start and end tags.
pub fn write_body(out: &mut String, el: &Element, path: &str, layout: &Layout) {
    let element_only = el.is_element_only();
    for c in &el.children {
        match c {
            Node::Element(e) => {
                let p = child_path(path, &e.name);
                if element_only {
                    out.push_str(Layout::at(&layout.open, &p));
                }
                write_element(out, e, &p, layout);
            }
            Node::Text(t) => out.push_str(t),
            Node::CData(t) => {
                out.push_str("<![CDATA[");
                out.push_str(t);
                out.push_str("]]>");
            }
            Node::Pi(p) => out.push_str(p),
        }
    }
    if element_only {
        out.push_str(Layout::at(&layout.close, path));
    }
}

/// Nodes with no whitespace between elements: what a row stores.
pub fn write_compact(nodes: &[Node]) -> String {
    let mut out = String::new();
    let layout = Layout::default();
    let holder = Element {
        children: nodes.to_vec(),
        ..Element::default()
    };
    write_body(&mut out, &holder, "", &layout);
    out
}

/// [`write_compact`] with every element's attributes in name order: two
/// trees that differ only in what XML says does not matter (attribute
/// order, whitespace between elements, how an empty tag is spelled)
/// write the same string.
pub fn canonical(el: &Element) -> String {
    fn sorted(el: &Element) -> Element {
        let mut attrs = el.attrs.clone();
        attrs.sort();
        Element {
            name: el.name.clone(),
            attrs,
            children: el
                .children
                .iter()
                .map(|c| match c {
                    Node::Element(e) => Node::Element(sorted(e)),
                    other => other.clone(),
                })
                .collect(),
            self_closing: el.self_closing && el.children.is_empty(),
        }
    }
    let mut out = String::new();
    let mut e = sorted(el);
    // `<x/>` and `<x></x>` are one element.
    fn open_empties(e: &mut Element) {
        e.self_closing = false;
        for c in &mut e.children {
            if let Node::Element(c) = c {
                open_empties(c);
            }
        }
    }
    open_empties(&mut e);
    write_element(&mut out, &e, "", &Layout::default());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(src: &str) -> String {
        let doc = parse(src).unwrap();
        let mut out = doc.prolog.clone();
        if !doc.prolog.ends_with("/>") {
            write_body(&mut out, &doc.root, &doc.root.name, &doc.layout);
            out.push_str(&doc.epilog);
        } else {
            out.push_str(&doc.epilog);
        }
        out
    }

    #[test]
    fn indented_and_compact_writers_round_trip() {
        for src in [
            "<?xml version=\"1.0\"?>\n<gpx\n version=\"1.1\">\n  <trk>\n    <name>Away team</name>\n  </trk>\n</gpx>\n",
            "<gpx><trk><trkseg>\n<trkpt lat=\"1\" lon=\"2\"><ele>3</ele></trkpt>\n<trkpt lat=\"1\" lon=\"2\"><ele>3</ele></trkpt>\n</trkseg></trk></gpx>",
            "<gpx>\n<wpt lat=\"1\" lon=\"2\">\n<name><![CDATA[Ten Forward & bar]]></name>\n</wpt>\n</gpx>",
        ] {
            assert_eq!(round_trip(src), src);
        }
    }

    #[test]
    fn self_closing_style_is_remembered() {
        let src = "<gpx>\n  <e>\n    <x:m c=\"1\" />\n  </e>\n</gpx>";
        assert_eq!(round_trip(src), src);
        let doc = parse(src).unwrap();
        assert_eq!(doc.layout.self_close.as_deref(), Some(" />"));
    }

    #[test]
    fn references_and_whitespace_only_leaves_are_content() {
        let src = "<gpx>\n <desc>Picard &amp; Riker</desc>\n <cmt> </cmt>\n</gpx>";
        let doc = parse(src).unwrap();
        let Node::Element(desc) = &doc.root.children[0] else {
            panic!()
        };
        assert_eq!(desc.children, vec![Node::Text("Picard &amp; Riker".into())]);
        let Node::Element(cmt) = &doc.root.children[1] else {
            panic!()
        };
        assert_eq!(cmt.children, vec![Node::Text(" ".into())]);
        assert_eq!(round_trip(src), src);
    }

    /// My Tracks puts a track's `<extensions>` on one line and its points
    /// one per line, at the same depth: whitespace is per path.
    #[test]
    fn two_paths_at_one_depth_keep_their_own_whitespace() {
        let src = "<gpx>\n<trk>\n<extensions><color>c0</color></extensions>\n<trkseg>\n<trkpt lat=\"1\" lon=\"2\">\n<time>t</time>\n</trkpt>\n</trkseg>\n</trk>\n</gpx>";
        assert_eq!(round_trip(src), src);
    }

    #[test]
    fn comments_are_dropped() {
        let doc = parse("<gpx>\n<!-- engage -->\n<trk/>\n</gpx>").unwrap();
        assert_eq!(doc.root.children.len(), 1);
    }

    #[test]
    fn a_fragment_reads_back_what_compact_wrote() {
        let doc = parse("<gpx>\n <a k=\"v\">\n  <b>t &lt; u</b>\n  <c/>\n </a>\n</gpx>").unwrap();
        let compact = write_compact(&doc.root.children);
        assert_eq!(compact, "<a k=\"v\"><b>t &lt; u</b><c/></a>");
        assert_eq!(parse_fragment(&compact).unwrap(), doc.root.children);
    }

    #[test]
    fn canonical_ignores_attribute_order_and_empty_spelling() {
        let a = parse("<gpx><p lat=\"1\" lon=\"2\"/></gpx>").unwrap();
        let b = parse("<gpx>\n  <p lon=\"2\" lat=\"1\"></p>\n</gpx>").unwrap();
        assert_eq!(canonical(&a.root), canonical(&b.root));
    }

    #[test]
    fn attrs_read_back() {
        let attrs = vec![
            ("lon".to_string(), "2".to_string()),
            ("lat".into(), "1".into()),
        ];
        let mut s = String::new();
        write_attrs(&mut s, &attrs);
        assert_eq!(parse_attrs(&s).unwrap(), attrs);
    }
}
