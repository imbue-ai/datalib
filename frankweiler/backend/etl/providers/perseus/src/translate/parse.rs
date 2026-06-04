//! TEI XML walker.
//!
//! The TEI shape we care about, after `<text>/<body>/<div type="edition">`:
//!
//! ```xml
//! <div subtype="book" n="1">
//!   <div subtype="chapter" n="1">
//!     <div subtype="section" n="1">…text…</div>
//!     <div subtype="section" n="2">…text…</div>
//!   </div>
//!   …
//! </div>
//! ```
//!
//! Some chapters in the older editions have no `<div subtype="section">`
//! children — the chapter `<div>` carries the text directly. We fall
//! back to `n="1"` for those, matching the Python script's behavior so
//! UUIDs line up against pre-existing data roots.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::{ENG_FILENAME, GRC_FILENAME};

/// One section's plain text, aligned across both language editions.
#[derive(Debug, Clone, Default)]
pub struct Section {
    pub n: String,
    pub grc: String,
    pub eng: String,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub book_n: String,
    pub n: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone)]
pub struct Book {
    pub n: String,
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedPerseus {
    pub books: Vec<Book>,
}

/// Read both TEI XMLs from `input_path` and align by (book, chapter,
/// section). Greek is the spine; English fills in where present.
pub fn parse(input_path: &Path) -> Result<ParsedPerseus> {
    let grc_path = input_path.join(GRC_FILENAME);
    let eng_path = input_path.join(ENG_FILENAME);
    if !grc_path.exists() || !eng_path.exists() {
        anyhow::bail!(
            "expected {GRC_FILENAME} and {ENG_FILENAME} under {} — see frankweiler_etl_perseus crate docs for the curl invocation",
            input_path.display()
        );
    }
    let grc = parse_one(&grc_path).with_context(|| format!("parsing {}", grc_path.display()))?;
    let eng = parse_one(&eng_path).with_context(|| format!("parsing {}", eng_path.display()))?;
    Ok(align(grc, eng))
}

/// (book_n, chapter_n, section_n) → plain text. BTreeMap so iteration
/// order is deterministic; that gives us stable rendered output.
type FlatMap = BTreeMap<(String, String, String), String>;

fn parse_one(path: &Path) -> Result<FlatMap> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(false);

    let mut out: FlatMap = BTreeMap::new();

    // Stack of currently-open `<div>` elements, each as
    // (subtype, n, accumulated_text, had_section_child).
    let mut stack: Vec<DivFrame> = Vec::new();
    // Skip everything up to the first <text><body><div…> chain — TEI
    // headers have prose we don't want to mix into the body.
    let mut in_body = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(&e);
                if !in_body {
                    if name == "body" {
                        in_body = true;
                    }
                    continue;
                }
                if name == "div" {
                    stack.push(DivFrame::from_start(&e));
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name_end(&e);
                if !in_body {
                    continue;
                }
                if name == "body" {
                    break;
                }
                if name == "div" {
                    if let Some(frame) = stack.pop() {
                        flush_frame(&mut stack, frame, &mut out);
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if !in_body {
                    continue;
                }
                if let Some(top) = stack.last_mut() {
                    let decoded =
                        t.unescape().with_context(|| "decoding TEI text node")?;
                    push_normalized(&mut top.text, &decoded);
                }
            }
            Ok(Event::CData(c)) => {
                if !in_body {
                    continue;
                }
                if let Some(top) = stack.last_mut() {
                    let s = std::str::from_utf8(&c).unwrap_or("");
                    push_normalized(&mut top.text, s);
                }
            }
            Ok(Event::Empty(_)) => { /* self-closing tags carry no text we need */ }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => anyhow::bail!("XML error at offset {}: {e}", reader.buffer_position()),
        }
        buf.clear();
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct DivFrame {
    subtype: String,
    n: String,
    text: String,
    had_section_child: bool,
}

impl DivFrame {
    fn from_start(e: &BytesStart) -> Self {
        let mut f = DivFrame::default();
        for attr in e.attributes().flatten() {
            let key = attr.key.as_ref();
            let val = attr.unescape_value().map(|v| v.into_owned()).unwrap_or_default();
            // attr keys are e.g. b"subtype" or b"n" (no namespace
            // prefix in PerseusDL editions).
            if key == b"subtype" {
                f.subtype = val;
            } else if key == b"n" {
                f.n = val;
            }
        }
        f
    }
}

fn flush_frame(stack: &mut [DivFrame], frame: DivFrame, out: &mut FlatMap) {
    match frame.subtype.as_str() {
        "section" => {
            // Find the chapter + book up the stack.
            let mut book_n = String::new();
            let mut chap_n = String::new();
            for f in stack.iter().rev() {
                if f.subtype == "chapter" && chap_n.is_empty() {
                    chap_n = f.n.clone();
                }
                if f.subtype == "book" && book_n.is_empty() {
                    book_n = f.n.clone();
                    break;
                }
            }
            if !book_n.is_empty() && !chap_n.is_empty() {
                out.insert(
                    (book_n, chap_n, frame.n.clone()),
                    normalize_whitespace(&frame.text),
                );
            }
            // Tell the ancestor chapter that it had a section child.
            if let Some(parent) = stack.iter_mut().rev().find(|f| f.subtype == "chapter") {
                parent.had_section_child = true;
            }
        }
        "chapter" => {
            // No section children → the chapter's own concatenated
            // text is section "1". This matches the Python fallback
            // path so UUIDs line up.
            if !frame.had_section_child {
                let book_n = stack
                    .iter()
                    .rev()
                    .find(|f| f.subtype == "book")
                    .map(|f| f.n.clone())
                    .unwrap_or_default();
                if !book_n.is_empty() {
                    out.insert(
                        (book_n, frame.n.clone(), "1".to_string()),
                        normalize_whitespace(&frame.text),
                    );
                }
            }
        }
        _ => {}
    }
}

fn local_name_bytes(name: &[u8]) -> &str {
    let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
    std::str::from_utf8(local).unwrap_or("")
}

fn local_name(e: &BytesStart<'_>) -> String {
    local_name_bytes(e.name().as_ref()).to_string()
}

fn local_name_end(e: &quick_xml::events::BytesEnd<'_>) -> String {
    local_name_bytes(e.name().as_ref()).to_string()
}

/// Stash the raw text; defer collapsing whitespace until `flush_frame`
/// so we don't lose word boundaries across child element gaps.
fn push_normalized(buf: &mut String, s: &str) {
    buf.push_str(s);
}

/// Match the Python script's `_norm_text` — collapse all whitespace
/// runs to single spaces and trim.
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = true; // suppress leading whitespace
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn align(grc: FlatMap, eng: FlatMap) -> ParsedPerseus {
    let mut books: Vec<Book> = Vec::new();
    // Walk Greek in its emit order — that's our spine.
    for ((bn, cn, sn), grc_text) in grc.iter() {
        let book = match books.last_mut() {
            Some(b) if b.n == *bn => b,
            _ => {
                books.push(Book {
                    n: bn.clone(),
                    chapters: Vec::new(),
                });
                books.last_mut().unwrap()
            }
        };
        let chapter = match book.chapters.last_mut() {
            Some(c) if c.n == *cn => c,
            _ => {
                book.chapters.push(Chapter {
                    book_n: bn.clone(),
                    n: cn.clone(),
                    sections: Vec::new(),
                });
                book.chapters.last_mut().unwrap()
            }
        };
        chapter.sections.push(Section {
            n: sn.clone(),
            grc: grc_text.clone(),
            eng: eng.get(&(bn.clone(), cn.clone(), sn.clone()))
                .cloned()
                .unwrap_or_default(),
        });
    }
    ParsedPerseus { books }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_GRC: &str = r#"<?xml version="1.0"?>
<TEI xmlns="http://www.tei-c.org/ns/1.0">
  <text>
    <body>
      <div type="edition">
        <div subtype="book" n="1">
          <div subtype="chapter" n="1">
            <div subtype="section" n="1">Θουκυδίδης Ἀθηναῖος ξυνέγραψε.</div>
            <div subtype="section" n="2">τὸν πόλεμον τῶν Πελοποννησίων.</div>
          </div>
        </div>
      </div>
    </body>
  </text>
</TEI>
"#;

    #[test]
    fn parses_two_sections() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(GRC_FILENAME), TINY_GRC).unwrap();
        // English file can be a minimal stub; alignment falls back to empty.
        let tiny_eng = TINY_GRC
            .replace("Θουκυδίδης Ἀθηναῖος ξυνέγραψε.", "Thucydides the Athenian wrote.")
            .replace(
                "τὸν πόλεμον τῶν Πελοποννησίων.",
                "the war of the Peloponnesians.",
            );
        std::fs::write(dir.path().join(ENG_FILENAME), tiny_eng).unwrap();

        let parsed = parse(dir.path()).unwrap();
        assert_eq!(parsed.books.len(), 1);
        assert_eq!(parsed.books[0].n, "1");
        assert_eq!(parsed.books[0].chapters.len(), 1);
        let secs = &parsed.books[0].chapters[0].sections;
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].n, "1");
        assert_eq!(secs[0].grc, "Θουκυδίδης Ἀθηναῖος ξυνέγραψε.");
        assert_eq!(secs[0].eng, "Thucydides the Athenian wrote.");
    }

    #[test]
    fn chapter_with_no_sections_falls_back_to_section_1() {
        let dir = tempfile::tempdir().unwrap();
        let grc = r#"<?xml version="1.0"?>
<TEI xmlns="http://www.tei-c.org/ns/1.0">
  <text>
    <body>
      <div type="edition">
        <div subtype="book" n="1">
          <div subtype="chapter" n="1">Bare chapter text with no section divs.</div>
        </div>
      </div>
    </body>
  </text>
</TEI>"#;
        std::fs::write(dir.path().join(GRC_FILENAME), grc).unwrap();
        std::fs::write(dir.path().join(ENG_FILENAME), grc).unwrap();
        let parsed = parse(dir.path()).unwrap();
        let secs = &parsed.books[0].chapters[0].sections;
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].n, "1");
        assert!(secs[0].grc.contains("Bare chapter text"));
    }

    #[test]
    fn whitespace_is_collapsed_across_text_runs() {
        let got = normalize_whitespace("  foo   bar\n\n  baz  ");
        assert_eq!(got, "foo bar baz");
    }
}
