//! `Google Play Books/<book>/<book>.html`: one page per book, a row per
//! highlight, note, bookmark or listening position on it.
//!
//! The page is a header (title, author, narrator, a link to the book)
//! and sections headed `My notes`, `My bookmarks` or `Listening
//! History`, each a list of entries whose parts are `div`s named by
//! class (`book-text`, `annotation-text`, `last-modified-date`). An
//! entry is kept as those parts by class, so render can say which it
//! does not read. The book itself (a PDF or EPUB) is left out.

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde_json::{json, Map, Value};

use super::db::RawDb;
use super::per_file::{self, Counts, Feed, FromFile, Occurrences};
use super::schema_raw::{ns_id, PlayBooksAnnotationRow};
use super::time;

pub const FEED: Feed = Feed {
    name: "play_books",
    dir: "Google Play Books",
    reads: |rel| rel.to_ascii_lowercase().ends_with(".html"),
};

impl FromFile for PlayBooksAnnotationRow {
    fn file(&self) -> &str {
        &self.file
    }
}

/// The `div`s an entry starts with.
const ENTRY_CLASSES: &[&str] = &["annotation", "document-position"];

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<Counts> {
    let n = per_file::ingest(db, scan, &FEED, found, |f, bytes| {
        let html = std::str::from_utf8(bytes).context("the book's page is not UTF-8")?;
        parse(&f.rel, html)
    })
    .await?;
    progress.set_message(&format!("play_books: {}", n.written));
    Ok(n)
}

fn class_of(e: &BytesStart) -> Vec<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == "class")
        .and_then(|a| {
            datalib_etl::xml::attr_value(&a)
                .ok()
                .map(|c| c.into_owned())
        })
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn href_of(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == "href")
        .and_then(|a| {
            datalib_etl::xml::attr_value(&a)
                .ok()
                .map(|c| c.into_owned())
        })
}

/// Where text read now goes.
#[derive(Clone)]
enum Into_ {
    Book(String),
    Section,
    Entry(String),
    Nowhere,
}

struct Frame {
    tag: String,
    into: Into_,
}

pub fn parse(rel: &str, html: &str) -> Result<Vec<PlayBooksAnnotationRow>> {
    let mut r = Reader::from_str(html);
    r.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut book: Map<String, Value> = Map::new();
    let mut links: Vec<Value> = Vec::new();
    let mut section = String::new();
    let mut entries: Vec<(String, String, Map<String, Value>)> = Vec::new();
    let mut in_body = false;

    // Text is kept as written, a line break at a `<br/>` and at the end
    // of each element; `tidy` trims the lines afterwards. A character
    // reference (`&#8239;`) is text of its own, in the middle of a line.
    let push_text = |m: &mut Map<String, Value>, key: &str, text: &str| {
        let prev = m.get(key).and_then(Value::as_str).unwrap_or("");
        m.insert(key.to_string(), Value::String(format!("{prev}{text}")));
    };

    loop {
        let event = r
            .read_event_into(&mut buf)
            .context("read the book's page")?;
        match event {
            Event::Start(e) => {
                let tag = e.name().as_ref().to_string();
                let classes = class_of(&e);
                let parent = stack
                    .last()
                    .map(|f| f.into.clone())
                    .unwrap_or(Into_::Nowhere);
                if tag == "body" {
                    in_body = true;
                }
                let into = if !in_body {
                    Into_::Nowhere
                } else if tag == "h2" {
                    section.clear();
                    Into_::Section
                } else if tag == "div"
                    && classes.iter().any(|c| ENTRY_CLASSES.contains(&c.as_str()))
                {
                    let kind = classes
                        .iter()
                        .find(|c| ENTRY_CLASSES.contains(&c.as_str()))
                        .cloned()
                        .unwrap_or_default();
                    entries.push((section.trim().to_string(), kind.clone(), Map::new()));
                    Into_::Entry(kind)
                } else if let Into_::Entry(_) = parent {
                    let key = classes.first().cloned().unwrap_or_else(|| tag.clone());
                    if tag == "a" {
                        if let (Some(h), Some(entry)) = (href_of(&e), entries.last_mut()) {
                            entry.2.insert("link".to_string(), Value::String(h));
                        }
                    }
                    Into_::Entry(key)
                } else if tag == "h1" {
                    Into_::Book("title".to_string())
                } else if let Some(c) = classes.first().filter(|c| {
                    !matches!(
                        c.as_str(),
                        "content"
                            | "header"
                            | "book-meta-information"
                            | "annotations"
                            | "document-positions"
                    )
                }) {
                    if tag == "a" {
                        if let Some(h) = href_of(&e) {
                            links.push(json!({"class": c, "href": h}));
                        }
                    }
                    Into_::Book(c.clone())
                } else {
                    parent
                };
                stack.push(Frame { tag, into });
            }
            Event::Empty(e) if e.name().as_ref() == "br" => match stack.last().map(|f| &f.into) {
                Some(Into_::Book(key)) => push_text(&mut book, key, "\n"),
                Some(Into_::Entry(key)) => {
                    if let Some(entry) = entries.last_mut() {
                        push_text(&mut entry.2, key, "\n");
                    }
                }
                _ => {}
            },
            Event::End(e) => {
                let tag = e.name().as_ref().to_string();
                if let Some(at) = stack.iter().rposition(|f| f.tag == tag) {
                    match &stack[at].into {
                        Into_::Book(key) => push_text(&mut book, key, "\n"),
                        Into_::Entry(key) => {
                            if let Some(entry) = entries.last_mut() {
                                push_text(&mut entry.2, key, "\n");
                            }
                        }
                        _ => {}
                    }
                    stack.truncate(at);
                }
            }
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) => {
                let text = match &event {
                    Event::Text(t) => t.to_string(),
                    Event::GeneralRef(g) => datalib_etl::xml::reference_text(g, true),
                    Event::CData(c) => c.to_string(),
                    _ => unreachable!(),
                };
                match stack.last().map(|f| &f.into) {
                    Some(Into_::Book(key)) => push_text(&mut book, key, &text),
                    Some(Into_::Section) => section.push_str(&text),
                    Some(Into_::Entry(key)) => {
                        if let Some(entry) = entries.last_mut() {
                            push_text(&mut entry.2, key, &text);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    tidy(&mut book);
    for entry in &mut entries {
        tidy(&mut entry.2);
    }
    if !links.is_empty() {
        book.insert("links".to_string(), Value::Array(links));
    }
    if book.get("title").is_none() && entries.is_empty() {
        return Err(super::unknown_layout(
            &super::shape::shape_rel(rel),
            "has no title and no entries",
        ));
    }

    let mut seen = Occurrences::default();
    let book = Value::Object(book);
    let rows = entries
        .into_iter()
        .enumerate()
        .map(|(position, (section, kind, fields))| {
            let get = |k: &str| fields.get(k).and_then(Value::as_str).unwrap_or("");
            let when_raw = get("last-modified-date");
            let when_ts = time::parse_play_books_stamp(when_raw);
            let natural = [
                section.as_str(),
                kind.as_str(),
                get("book-text"),
                get("annotation-text"),
                get("document-position-text"),
                when_raw,
            ]
            .join("\u{1f}");
            let n = seen.next(&natural);
            let id = ns_id(&format!(
                "play_books:{rel}:{}:{n}",
                blake3::hash(natural.as_bytes()).to_hex()
            ));
            let payload = json!({
                "book": book,
                "section": section,
                "kind": kind,
                "fields": Value::Object(fields),
                "position": position,
            });
            PlayBooksAnnotationRow {
                id_and_payload: WirePayload {
                    id,
                    payload: payload.to_string(),
                },
                file: rel.to_string(),
                when_ts,
            }
        })
        .collect();
    Ok(rows)
}

/// Each value's lines trimmed, the empty ones dropped; a value left
/// with nothing is no value.
fn tidy(m: &mut Map<String, Value>) {
    let keys: Vec<String> = m.keys().cloned().collect();
    for k in keys {
        let text = m.get(&k).and_then(Value::as_str).unwrap_or("");
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            m.remove(&k);
        } else {
            m.insert(k, Value::String(lines.join("\n")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const PAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>The Art of Diplomacy</title><style type="text/css">.annotation { margin: 1em; }</style></head>
<body><div class="content"><div class="header"><h1>The Art of Diplomacy</h1><div class="author">Jean-Luc Picard</div></div>
<div class="book-meta-information"><a class="meta-entry" href="https://play.google.com/store/books/details?id=TNGdipl01">View in Google Play</a><div class="meta-entry">Starfleet Academy Press</div></div>
<div class="annotations"><h2>My notes</h2>
<div class="annotation"><div class="book-text">Make it so.</div><div class="annotation-text">The captain's motto.</div>
<div class="last-modified-date">Last modified on<br/>Jan 12, 2364, 9:41:00&#8239;AM<br/>Pacific Time</div><div class="collection"></div></div>
<div class="annotation"><div class="book-text">Tea, Earl Grey, hot.</div><div class="annotation-text"></div>
<div class="last-modified-date">Last modified on<br/>Feb 4, 2364, 6:00:00&#8239;PM<br/>Pacific Time</div><div class="collection"></div></div>
</div></div></body></html>"#;

    fn payloads(rows: &[PlayBooksAnnotationRow]) -> Vec<Value> {
        rows.iter()
            .map(|r| serde_json::from_str(&r.id_and_payload.payload).unwrap())
            .collect()
    }

    #[test]
    fn every_entry_with_its_parts_by_class() {
        let rows = parse("Google Play Books/Diplomacy/Diplomacy.html", PAGE).unwrap();
        let p = payloads(&rows);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0]["book"]["title"], "The Art of Diplomacy");
        assert_eq!(p[0]["book"]["author"], "Jean-Luc Picard");
        assert_eq!(
            p[0]["book"]["meta-entry"],
            "View in Google Play\nStarfleet Academy Press"
        );
        assert_eq!(p[0]["section"], "My notes");
        assert_eq!(p[0]["fields"]["book-text"], "Make it so.");
        assert_eq!(p[0]["fields"]["annotation-text"], "The captain's motto.");
        assert!(
            p[1]["fields"].get("annotation-text").is_none(),
            "an empty note"
        );
        assert_eq!(
            rows[0].when_ts.as_deref(),
            Some("2364-01-12T09:41:00-08:00")
        );
        assert_eq!(
            rows[1].when_ts.as_deref(),
            Some("2364-02-04T18:00:00-08:00")
        );
    }
}
