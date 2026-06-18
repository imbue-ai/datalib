//! quick-xml pull-parser for "SMS Backup & Restore" export files.
//!
//! The app emits two flat, attribute-heavy XML shapes:
//!
//! ```xml
//! <smses count="N" ...>
//!   <sms protocol="0" address="+1555" date="1778277131098" type="2"
//!        body="Hello" date_sent="0" readable_date="..." contact_name="..." />
//!   <mms date="1781547510000" msg_box="2" address="+1555" m_id="T19ec..."
//!        tr_id="proto:..." ...>
//!     <parts>
//!       <part seq="-1" ct="application/smil" .../>          <!-- layout, skipped -->
//!       <part seq="0"  ct="image/jpeg" cl="image000000.jpg" data="<base64>" />
//!       <part seq="0"  ct="text/plain" text="caption" />     <!-- body text -->
//!     </parts>
//!     <addrs>
//!       <addr address="+1555" type="151" charset="106" />
//!     </addrs>
//!   </mms>
//! </smses>
//! ```
//!
//! and `<calls><call number=".." duration=".." date=".." type=".." /></calls>`.
//!
//! All attribute values arrive XML-escaped (`&lt;`, `&#10;`, emoji as
//! `&#129310;`); [`quick_xml`]'s `unescape_value` decodes them, so the
//! parsed structs carry real text. `count`/`backup_set`/etc. on the root
//! are ignored — we trust the records themselves.

use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// Which kind of export a file is, sniffed from its root element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Smses,
    Calls,
}

/// One `<sms>` record.
#[derive(Debug, Clone, Default)]
pub struct SmsRecord {
    pub address: String,
    pub date_ms: i64,
    /// 1 = received (inbox), 2 = sent. Other values pass through.
    pub type_: i64,
    pub body: String,
    pub date_sent_ms: Option<i64>,
    pub readable_date: Option<String>,
    pub contact_name: Option<String>,
}

/// One decoded MMS part carrying real bytes (image / audio / …).
#[derive(Debug, Clone)]
pub struct MmsBlob {
    /// The part's filename (`cl` or `name`), e.g. `image000000.jpg`.
    pub name: String,
    /// The part's content type (`ct`), e.g. `image/jpeg`, `audio/mp4`.
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// One `<mms>` record.
#[derive(Debug, Clone, Default)]
pub struct MmsRecord {
    pub address: String,
    pub date_ms: i64,
    /// 1 = received (inbox), 2 = sent.
    pub msg_box: i64,
    pub m_id: Option<String>,
    pub tr_id: Option<String>,
    pub date_sent_ms: Option<i64>,
    pub readable_date: Option<String>,
    pub contact_name: Option<String>,
    /// Concatenated `text/plain` part text (the human-readable body).
    pub text: String,
    /// Image / audio / video part blobs.
    pub blobs: Vec<MmsBlob>,
}

/// One `<call>` record.
#[derive(Debug, Clone, Default)]
pub struct CallRecord {
    pub number: String,
    pub duration_s: i64,
    pub date_ms: i64,
    /// 1 incoming, 2 outgoing, 3 missed, 4 voicemail, 5 rejected, 6 blocked.
    pub type_: i64,
    pub readable_date: Option<String>,
    pub contact_name: Option<String>,
}

/// Sniff a file's root element. Returns `None` for anything that isn't
/// a recognized SMS Backup & Restore export.
pub fn detect_root(xml: &str) -> Option<RootKind> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                return match e.name().as_ref() {
                    b"smses" => Some(RootKind::Smses),
                    b"calls" => Some(RootKind::Calls),
                    _ => None,
                };
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Parse a `<smses>` file into its `<sms>` and `<mms>` records.
pub fn parse_smses(xml: &str) -> Result<(Vec<SmsRecord>, Vec<MmsRecord>)> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut smses = Vec::new();
    let mut mmses = Vec::new();

    loop {
        match reader.read_event_into(&mut buf).context("read xml event")? {
            Event::Empty(e) if e.name().as_ref() == b"sms" => {
                smses.push(sms_from_attrs(&attrs(&e)?));
            }
            // An `<sms>` is normally self-closing; tolerate a Start form.
            Event::Start(e) if e.name().as_ref() == b"sms" => {
                smses.push(sms_from_attrs(&attrs(&e)?));
            }
            Event::Start(e) if e.name().as_ref() == b"mms" => {
                let head = attrs(&e)?;
                mmses.push(parse_mms_body(&mut reader, head)?);
            }
            Event::Empty(e) if e.name().as_ref() == b"mms" => {
                // An MMS with no parts (rare) — header only.
                mmses.push(mms_from_attrs(&attrs(&e)?));
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((smses, mmses))
}

/// Parse a `<calls>` file into its `<call>` records.
pub fn parse_calls(xml: &str) -> Result<Vec<CallRecord>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut calls = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).context("read xml event")? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"call" => {
                let a = attrs(&e)?;
                calls.push(CallRecord {
                    number: a.get("number").cloned().unwrap_or_default(),
                    duration_s: int(&a, "duration").unwrap_or(0),
                    date_ms: int(&a, "date").unwrap_or(0),
                    type_: int(&a, "type").unwrap_or(0),
                    readable_date: opt(&a, "readable_date"),
                    contact_name: opt(&a, "contact_name"),
                });
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(calls)
}

/// Read the children of an `<mms>` Start element (parts + addrs) until
/// its End, folding `text/plain` parts into the body and decoding
/// image/audio/video parts into [`MmsBlob`]s. `application/smil` (the
/// layout) is skipped.
fn parse_mms_body(reader: &mut Reader<&[u8]>, head: Attrs) -> Result<MmsRecord> {
    let mut rec = mms_from_attrs(&head);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).context("read mms child")? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"part" => {
                let a = attrs(&e)?;
                let ct = a.get("ct").map(String::as_str).unwrap_or("");
                if ct == "application/smil" {
                    // Layout descriptor — not content.
                } else if ct == "text/plain" {
                    if let Some(t) = opt(&a, "text") {
                        if !rec.text.is_empty() {
                            rec.text.push('\n');
                        }
                        rec.text.push_str(t.trim_end());
                    }
                } else if let Some(b64) = opt(&a, "data") {
                    // image/* | audio/* | video/* | … — decode the bytes.
                    match decode_base64(&b64) {
                        Ok(bytes) => rec.blobs.push(MmsBlob {
                            name: part_name(&a, ct, rec.blobs.len()),
                            content_type: ct.to_string(),
                            bytes,
                        }),
                        Err(e) => {
                            tracing::warn!(
                                event = "sms_mms_part_base64_failed",
                                ct,
                                error = %e,
                            );
                        }
                    }
                }
            }
            Event::End(e) if e.name().as_ref() == b"mms" => break,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(rec)
}

/// The display filename for a content part: prefer `cl`, then `name`,
/// else synthesize `part{idx}.<ext-from-ct>`.
fn part_name(a: &Attrs, ct: &str, idx: usize) -> String {
    if let Some(cl) = opt(a, "cl") {
        return cl;
    }
    if let Some(name) = opt(a, "name") {
        return name;
    }
    let ext = ct.rsplit_once('/').map(|(_, s)| s).unwrap_or("bin");
    format!("part{idx}.{ext}")
}

fn sms_from_attrs(a: &Attrs) -> SmsRecord {
    SmsRecord {
        address: a.get("address").cloned().unwrap_or_default(),
        date_ms: int(a, "date").unwrap_or(0),
        type_: int(a, "type").unwrap_or(0),
        body: a.get("body").cloned().unwrap_or_default(),
        date_sent_ms: int(a, "date_sent"),
        readable_date: opt(a, "readable_date"),
        contact_name: opt(a, "contact_name"),
    }
}

fn mms_from_attrs(a: &Attrs) -> MmsRecord {
    MmsRecord {
        address: a.get("address").cloned().unwrap_or_default(),
        date_ms: int(a, "date").unwrap_or(0),
        msg_box: int(a, "msg_box").unwrap_or(0),
        m_id: opt(a, "m_id"),
        tr_id: opt(a, "tr_id"),
        date_sent_ms: int(a, "date_sent"),
        readable_date: opt(a, "readable_date"),
        contact_name: opt(a, "contact_name"),
        text: String::new(),
        blobs: Vec::new(),
    }
}

type Attrs = HashMap<String, String>;

/// Collect an element's attributes into a map of unescaped strings.
fn attrs(e: &BytesStart) -> Result<Attrs> {
    let mut map = HashMap::new();
    for attr in e.attributes() {
        let attr = attr.context("parse attribute")?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let val = attr
            .unescape_value()
            .context("unescape attribute")?
            .into_owned();
        map.insert(key, val);
    }
    Ok(map)
}

/// A present, non-"null", non-empty attribute. The app writes the
/// literal string `null` for absent values.
fn opt(a: &Attrs, key: &str) -> Option<String> {
    a.get(key)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(str::to_string)
}

/// Parse an integer-valued attribute (`date`, `type`, `duration`, …),
/// `None` when absent / non-numeric / "null".
fn int(a: &Attrs, key: &str) -> Option<i64> {
    opt(a, key).and_then(|s| s.trim().parse::<i64>().ok())
}

/// Decode a base64 part payload, tolerating embedded whitespace.
fn decode_base64(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .context("base64 decode")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMSES: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<smses count="2">
  <sms protocol="0" address="+17783176760" date="1778277198761" type="1" body="Hello, right back at you!" date_sent="1778277198000" readable_date="May 8, 2026 2:53:18 p.m." contact_name="(Unknown)" />
  <sms protocol="0" address="+12262121542" date="1778277388135" type="1" body="&lt;#&gt; code 763&#10;line two" date_sent="0" readable_date="x" contact_name="null" />
</smses>"#;

    #[test]
    fn detect_root_kinds() {
        assert_eq!(detect_root(SMSES), Some(RootKind::Smses));
        assert_eq!(
            detect_root("<calls count=\"0\"></calls>"),
            Some(RootKind::Calls)
        );
        assert_eq!(detect_root("<other/>"), None);
    }

    #[test]
    fn parses_sms_with_unescaping() {
        let (sms, mms) = parse_smses(SMSES).unwrap();
        assert_eq!(mms.len(), 0);
        assert_eq!(sms.len(), 2);
        assert_eq!(sms[0].address, "+17783176760");
        assert_eq!(sms[0].type_, 1);
        assert_eq!(sms[0].body, "Hello, right back at you!");
        // Entities + numeric char refs are unescaped.
        assert_eq!(sms[1].body, "<#> code 763\nline two");
        // "null" contact_name collapses to None.
        assert_eq!(sms[1].contact_name, None);
        assert_eq!(sms[0].contact_name.as_deref(), Some("(Unknown)"));
    }

    #[test]
    fn parses_mms_parts_and_attachment() {
        // 1x1 transparent GIF, base64.
        let gif = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        let xml = format!(
            r#"<smses count="1">
  <mms date="1781811656000" msg_box="2" address="+17783176760" m_id="T19edc4037e2" tr_id="proto:abc">
    <parts>
      <part seq="-1" ct="application/smil" text="&lt;smil&gt;layout&lt;/smil&gt;" />
      <part seq="0" ct="image/gif" cl="image000001.gif" data="{gif}" />
      <part seq="0" ct="text/plain" text="Happy Thurs " />
    </parts>
    <addrs>
      <addr address="+17783176760" type="151" charset="106" />
    </addrs>
  </mms>
</smses>"#
        );
        let (sms, mms) = parse_smses(&xml).unwrap();
        assert_eq!(sms.len(), 0);
        assert_eq!(mms.len(), 1);
        let m = &mms[0];
        assert_eq!(m.msg_box, 2);
        assert_eq!(m.m_id.as_deref(), Some("T19edc4037e2"));
        // SMIL layout is skipped, text/plain becomes the body.
        assert_eq!(m.text, "Happy Thurs");
        // Exactly one decoded blob (the gif).
        assert_eq!(m.blobs.len(), 1);
        assert_eq!(m.blobs[0].name, "image000001.gif");
        assert_eq!(m.blobs[0].content_type, "image/gif");
        assert_eq!(&m.blobs[0].bytes[0..3], b"GIF");
    }

    #[test]
    fn parses_calls() {
        let xml = r#"<calls count="1">
  <call number="+16474495789" duration="42" date="1778698683617" type="3" readable_date="May 13" contact_name="(Unknown)" />
</calls>"#;
        let calls = parse_calls(xml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].number, "+16474495789");
        assert_eq!(calls[0].duration_s, 42);
        assert_eq!(calls[0].type_, 3);
    }
}
