//! TEI XML walker for the Perseus multi-edition corpus.
//!
//! Each edition (one TEI file per `tlg0003.tlg001.<id>.xml`) shares the
//! same locator scheme, after `<text>/<body>/<div type="edition">`:
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
//! back to `n="1"` for those.
//!
//! We no longer privilege a single Greek/English pair: every `*.xml`
//! under `input_path` (except `__cts__.xml`) is parsed as an edition,
//! and the rendered locator tree is the *union* of every edition's
//! (book, chapter, section) locators — a partial edition (e.g. a
//! German selection of a few speeches) simply contributes text for the
//! sections it covers. Human-readable edition titles come from
//! `__cts__.xml` when present.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::super::{cts_urn, TLG_FILE_PREFIX};

/// One published edition/translation of the work (one TEI file).
#[derive(Debug, Clone)]
pub struct Edition {
    /// Full edition id — the TEI basename minus the work prefix and
    /// `.xml` suffix, e.g. `perseus-grc2`, `1st1K-eng1`. This is the
    /// "version" key embedded in the grid rows' `kind` and in the
    /// per-(chapter, edition) UUIDs.
    pub id: String,
    /// Short, unique-per-work label — the id's suffix after the last
    /// `-`, e.g. `grc2`, `eng1`, `fre2`. Used as the toggle label.
    pub short: String,
    /// ISO-ish language code derived from `short` (its alphabetic
    /// prefix): `grc`, `eng`, `fre`, `ger`, `lat`, `ita`.
    pub lang: String,
    /// Human-readable title from `__cts__.xml` (label + translator),
    /// falling back to `short` when no CTS entry is present.
    pub title: String,
}

/// One section's text across the editions that cover it. Keyed by
/// [`Edition::id`]; absent keys = the edition doesn't render this
/// section.
#[derive(Debug, Clone, Default)]
pub struct Section {
    pub n: String,
    pub texts: BTreeMap<String, String>,
}

impl Section {
    /// Text for one edition, or "" when this edition doesn't cover the
    /// section.
    pub fn text(&self, edition_id: &str) -> &str {
        self.texts.get(edition_id).map(String::as_str).unwrap_or("")
    }
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
    pub editions: Vec<Edition>,
    pub books: Vec<Book>,
}

impl ParsedPerseus {
    /// Language code for an edition id, or "" if unknown. Used by the
    /// aligner to pick the right sentence splitter.
    pub fn lang_of(&self, edition_id: &str) -> &str {
        self.editions
            .iter()
            .find(|e| e.id == edition_id)
            .map(|e| e.lang.as_str())
            .unwrap_or("")
    }
}

/// Read every edition TEI under `input_path` and merge them into one
/// locator tree. Edition titles come from `__cts__.xml` when present.
pub fn parse(input_path: &Path) -> Result<ParsedPerseus> {
    let cts = read_cts(input_path)?; // id -> (lang_attr, title)

    // Discover edition files: `<TLG_FILE_PREFIX><id>.xml`, skipping the
    // CTS metadata and any other non-edition file.
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in
        fs::read_dir(input_path).with_context(|| format!("read_dir {}", input_path.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "__cts__.xml" || !name.ends_with(".xml") {
            continue;
        }
        let Some(id) = name
            .strip_prefix(TLG_FILE_PREFIX)
            .and_then(|s| s.strip_suffix(".xml"))
        else {
            continue;
        };
        files.push((id.to_string(), path));
    }
    if files.is_empty() {
        anyhow::bail!(
            "no `{TLG_FILE_PREFIX}*.xml` editions under {} — see frankweiler_etl_perseus crate docs",
            input_path.display()
        );
    }
    files.sort();

    // Parse each edition's text and build its metadata.
    let mut maps: Vec<(String, FlatMap)> = Vec::with_capacity(files.len());
    let mut editions: Vec<Edition> = Vec::with_capacity(files.len());
    for (id, path) in &files {
        let map = parse_one(path).with_context(|| format!("parsing {}", path.display()))?;
        let short = id.rsplit('-').next().unwrap_or(id).to_string();
        let lang = short
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .to_string();
        let title = cts
            .get(id)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| short.clone());
        editions.push(Edition {
            id: id.clone(),
            short,
            lang,
            title,
        });
        maps.push((id.clone(), map));
    }

    // Greek editions first (the original), then by short label.
    editions.sort_by(|a, b| (a.lang != "grc", &a.short).cmp(&(b.lang != "grc", &b.short)));

    // Union of every (book, chapter, section) locator, numerically
    // ordered, with per-edition text attached.
    let mut tree: BookTree = BTreeMap::new();
    for (id, map) in &maps {
        for ((bn, cn, sn), text) in map {
            if text.is_empty() {
                continue;
            }
            // Skip TEI front/back matter — title pages, prefaces,
            // indexes that some editions carry as `<div subtype="book"
            // n="front">` / `n="back">`. Thucydides' canonical CTS
            // locators are strictly numeric (book.chapter.section), so
            // anything non-numeric is editorial apparatus, not corpus
            // text, and would otherwise surface as a phantom "Book 0".
            if [bn, cn, sn].iter().any(|s| s.parse::<i64>().is_err()) {
                continue;
            }
            tree.entry(nkey(bn))
                .or_default()
                .entry(nkey(cn))
                .or_default()
                .entry(nkey(sn))
                .or_default()
                .insert(id.clone(), text.clone());
        }
    }

    let books = tree
        .into_iter()
        .map(|(bk, chaps)| Book {
            n: bk.1.clone(),
            chapters: chaps
                .into_iter()
                .map(|(ck, secs)| Chapter {
                    book_n: bk.1.clone(),
                    n: ck.1.clone(),
                    sections: secs
                        .into_iter()
                        .map(|(sk, texts)| Section { n: sk.1, texts })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    Ok(ParsedPerseus { editions, books })
}

/// Numeric-then-lexical sort key for the string locator components, so
/// chapter "10" sorts after "2", not before it.
type Key = (i64, String);
fn nkey(s: &str) -> Key {
    (s.parse::<i64>().unwrap_or(i64::MAX), s.to_string())
}

/// Nested locator union built during parse: book → chapter → section →
/// (edition id → text). Named to keep the type readable (and clippy
/// happy).
type SectionTexts = BTreeMap<String, String>;
type ChapterTree = BTreeMap<Key, SectionTexts>;
type BookChapters = BTreeMap<Key, ChapterTree>;
type BookTree = BTreeMap<Key, BookChapters>;

/// (book_n, chapter_n, section_n) → plain text. BTreeMap so iteration
/// order is deterministic.
type FlatMap = BTreeMap<(String, String, String), String>;

fn parse_one(path: &Path) -> Result<FlatMap> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(false);

    let mut out: FlatMap = BTreeMap::new();
    let mut stack: Vec<DivFrame> = Vec::new();
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
                    let decoded = t.unescape().with_context(|| "decoding TEI text node")?;
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
            Ok(Event::Empty(_)) => {}
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
            let val = attr
                .unescape_value()
                .map(|v| v.into_owned())
                .unwrap_or_default();
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
            if let Some(parent) = stack.iter_mut().rev().find(|f| f.subtype == "chapter") {
                parent.had_section_child = true;
            }
        }
        "chapter" if !frame.had_section_child => {
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
        _ => {}
    }
}

// ---- __cts__.xml metadata ----

/// Read `__cts__.xml` (if present) into `id -> (xml:lang, title)`. The
/// title is the CTS `<label>` plus the translator/editor surname pulled
/// from `<description>` when one can be found, which is what
/// distinguishes editions that share a generic label (e.g. the three
/// English "History of the Peloponnesian War" translations).
fn read_cts(input_path: &Path) -> Result<BTreeMap<String, (String, String)>> {
    let path = input_path.join("__cts__.xml");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(false);

    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut cur: Option<(String, String)> = None; // (id, lang)
    let mut label = String::new();
    let mut description = String::new();
    let mut sink = String::new(); // "label" | "description" | ""

    let prefix = format!("{}.", cts_urn());
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(&e);
                match name.as_str() {
                    "edition" | "translation" => {
                        let (urn, lang) = edition_attrs(&e);
                        let id = urn
                            .strip_prefix(&prefix)
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        cur = (!id.is_empty()).then_some((id, lang));
                        label.clear();
                        description.clear();
                    }
                    "label" if cur.is_some() => sink = "label".into(),
                    "description" if cur.is_some() => sink = "description".into(),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if !sink.is_empty() {
                    let decoded = t.unescape().unwrap_or_default();
                    let target = if sink == "label" {
                        &mut label
                    } else {
                        &mut description
                    };
                    target.push_str(&decoded);
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name_end(&e);
                match name.as_str() {
                    "label" | "description" => sink.clear(),
                    "edition" | "translation" => {
                        if let Some((id, lang)) = cur.take() {
                            let title = build_title(&label, &description);
                            out.insert(id, (lang, title));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => anyhow::bail!(
                "XML error in __cts__.xml at {}: {e}",
                reader.buffer_position()
            ),
        }
        buf.clear();
    }
    Ok(out)
}

/// Pull `urn` and `xml:lang` off an `<edition>`/`<translation>` start tag.
fn edition_attrs(e: &BytesStart) -> (String, String) {
    let mut urn = String::new();
    let mut lang = String::new();
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        let val = attr
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_default();
        if key == b"urn" {
            urn = val;
        } else if key.ends_with(b"lang") {
            lang = val;
        }
    }
    (urn, lang)
}

/// `<label>` + the attribution surname extracted from `<description>`.
fn build_title(label: &str, description: &str) -> String {
    let label = normalize_whitespace(label);
    let desc = normalize_whitespace(description);
    match attribution(&desc) {
        Some(name) if !label.is_empty() => format!("{label} — {name}"),
        Some(name) => name,
        None => label,
    }
}

/// Heuristic: a Perseus `<description>` reads
/// `"… <Surname>, <Given>, translator. <Place>: <Publisher>, <Year>."`.
/// Grab the surname (the token before the comma that precedes
/// "translator"/"editor"), which is what disambiguates editions.
fn attribution(desc: &str) -> Option<String> {
    for marker in [", translator", ", editor"] {
        if let Some(pos) = desc.find(marker) {
            let before = &desc[..pos];
            let clause_start = before.rfind(". ").map(|i| i + 2).unwrap_or(0);
            let clause = &before[clause_start..];
            let surname = clause.split(',').next().unwrap_or(clause).trim();
            if !surname.is_empty() {
                return Some(surname.to_string());
            }
        }
    }
    None
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

fn push_normalized(buf: &mut String, s: &str) {
    buf.push_str(s);
}

/// Collapse all whitespace runs to single spaces and trim.
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = true;
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

    fn write_edition(dir: &Path, id: &str, body: &str) {
        std::fs::write(dir.join(format!("{TLG_FILE_PREFIX}{id}.xml")), body).unwrap();
    }

    #[test]
    fn unions_editions_by_locator() {
        let dir = tempfile::tempdir().unwrap();
        write_edition(dir.path(), "perseus-grc2", TINY_GRC);
        let eng = TINY_GRC
            .replace("Θουκυδίδης Ἀθηναῖος ξυνέγραψε.", "Thucydides wrote.")
            .replace("τὸν πόλεμον τῶν Πελοποννησίων.", "the war.");
        write_edition(dir.path(), "1st1K-eng1", &eng);

        let parsed = parse(dir.path()).unwrap();
        assert_eq!(parsed.editions.len(), 2);
        // grc sorts first.
        assert_eq!(parsed.editions[0].id, "perseus-grc2");
        assert_eq!(parsed.editions[0].short, "grc2");
        assert_eq!(parsed.editions[0].lang, "grc");
        assert_eq!(parsed.editions[1].short, "eng1");

        assert_eq!(parsed.books.len(), 1);
        let secs = &parsed.books[0].chapters[0].sections;
        assert_eq!(secs.len(), 2);
        assert_eq!(
            secs[0].text("perseus-grc2"),
            "Θουκυδίδης Ἀθηναῖος ξυνέγραψε."
        );
        assert_eq!(secs[0].text("1st1K-eng1"), "Thucydides wrote.");
    }

    #[test]
    fn chapter_with_no_sections_falls_back_to_section_1() {
        let dir = tempfile::tempdir().unwrap();
        let grc = r#"<?xml version="1.0"?>
<TEI xmlns="http://www.tei-c.org/ns/1.0">
  <text><body><div type="edition">
    <div subtype="book" n="1">
      <div subtype="chapter" n="1">Bare chapter text with no section divs.</div>
    </div>
  </div></body></text>
</TEI>"#;
        write_edition(dir.path(), "perseus-grc2", grc);
        let parsed = parse(dir.path()).unwrap();
        let secs = &parsed.books[0].chapters[0].sections;
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].n, "1");
        assert!(secs[0].text("perseus-grc2").contains("Bare chapter text"));
    }

    #[test]
    fn chapters_sort_numerically() {
        let dir = tempfile::tempdir().unwrap();
        let grc = r#"<?xml version="1.0"?>
<TEI xmlns="http://www.tei-c.org/ns/1.0">
  <text><body><div type="edition">
    <div subtype="book" n="1">
      <div subtype="chapter" n="2"><div subtype="section" n="1">two</div></div>
      <div subtype="chapter" n="10"><div subtype="section" n="1">ten</div></div>
    </div>
  </div></body></text>
</TEI>"#;
        write_edition(dir.path(), "perseus-grc2", grc);
        let parsed = parse(dir.path()).unwrap();
        let chaps: Vec<&str> = parsed.books[0]
            .chapters
            .iter()
            .map(|c| c.n.as_str())
            .collect();
        assert_eq!(chaps, vec!["2", "10"]);
    }

    #[test]
    fn cts_titles_are_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write_edition(dir.path(), "perseus-eng6", TINY_GRC);
        let cts = r#"<?xml version="1.0"?>
<ti:work xmlns:ti="http://chs.harvard.edu/xmlns/cts" urn="urn:cts:greekLit:tlg0003.tlg001">
  <ti:translation urn="urn:cts:greekLit:tlg0003.tlg001.perseus-eng6" xml:lang="eng">
    <ti:label xml:lang="eng">History of the Peloponnesian War</ti:label>
    <ti:description xml:lang="eng">Thucydides. History of the Peloponnesian War. Crawley, Richard, translator. London, 1914.</ti:description>
  </ti:translation>
</ti:work>"#;
        std::fs::write(dir.path().join("__cts__.xml"), cts).unwrap();
        let parsed = parse(dir.path()).unwrap();
        assert_eq!(
            parsed.editions[0].title,
            "History of the Peloponnesian War — Crawley"
        );
    }

    #[test]
    fn whitespace_is_collapsed_across_text_runs() {
        let got = normalize_whitespace("  foo   bar\n\n  baz  ");
        assert_eq!(got, "foo bar baz");
    }
}
