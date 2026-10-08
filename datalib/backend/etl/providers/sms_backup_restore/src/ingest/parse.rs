//! quick-xml pull-parser for "SMS Backup & Restore" export files.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// Which kind of export a file is, sniffed from its root element.
/// `Other` is well-formed XML of some other kind, which a backup folder may
/// hold beside the backups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Smses,
    Calls,
    Other,
}

/// One `<sms>` record.
#[derive(Debug, Clone, Default)]
pub struct SmsRecord {
    pub address: String,
    pub date_ms: i64,
    /// 1 = received (inbox), 2 = sent. Other values pass through.
    pub type_: i64,
    pub body: String,
    /// Android's `read` column: whether the phone's owner has opened
    /// it. `None` when the backup does not say.
    pub read: Option<bool>,
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
    /// As [`SmsRecord::read`].
    pub read: Option<bool>,
    pub date_sent_ms: Option<i64>,
    pub readable_date: Option<String>,
    pub contact_name: Option<String>,
    /// Concatenated `text/plain` part text (the human-readable body).
    pub text: String,
    /// Image / audio / video part blobs.
    pub blobs: Vec<MmsBlob>,
    /// Parts whose bytes did not decode, as `(name, why)`.
    pub failed_blobs: Vec<(String, String)>,
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

/// An error when the file holds no element at all (0 bytes, bytes that are
/// not XML) or breaks before its first one: that is a backup that could not
/// be read, not a backup of nothing.
pub fn detect_root(xml: &str) -> Result<RootKind> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .context("read the root element")?
        {
            Event::Start(e) | Event::Empty(e) => {
                return Ok(match e.name().as_ref() {
                    b"smses" => RootKind::Smses,
                    b"calls" => RootKind::Calls,
                    _ => RootKind::Other,
                });
            }
            Event::Eof => bail!("the file holds no XML element"),
            _ => {}
        }
        buf.clear();
    }
}

/// A copy cut off part-way still parses up to where it stops, so a backup
/// counts as read only once its root element has closed.
fn ensure_closed(closed: bool, root: &str) -> Result<()> {
    if !closed {
        bail!("the file ends before its </{root}>: a copy cut off part-way");
    }
    Ok(())
}

pub fn parse_smses(xml: &str) -> Result<(Vec<SmsRecord>, Vec<MmsRecord>)> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut smses = Vec::new();
    let mut mmses = Vec::new();
    let mut closed = false;

    loop {
        match reader.read_event_into(&mut buf).context("read xml event")? {
            Event::Empty(e) if e.name().as_ref() == b"smses" => closed = true,
            Event::End(e) if e.name().as_ref() == b"smses" => closed = true,
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
    ensure_closed(closed, "smses")?;
    Ok((smses, mmses))
}

pub fn parse_calls(xml: &str) -> Result<Vec<CallRecord>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut calls = Vec::new();
    let mut closed = false;
    loop {
        match reader.read_event_into(&mut buf).context("read xml event")? {
            Event::Empty(e) if e.name().as_ref() == b"calls" => closed = true,
            Event::End(e) if e.name().as_ref() == b"calls" => closed = true,
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
    ensure_closed(closed, "calls")?;
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
                        // Named apart from the decoded parts, so a part after
                        // this one keeps the name it always had.
                        Err(e) => {
                            let name = opt(&a, "cl")
                                .or_else(|| opt(&a, "name"))
                                .unwrap_or_else(|| format!("undecoded{}", rec.failed_blobs.len()));
                            rec.failed_blobs.push((name, format!("{e:#}")))
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
        read: int(a, "read").map(|r| r != 0),
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
        read: int(a, "read").map(|r| r != 0),
        date_sent_ms: int(a, "date_sent"),
        readable_date: opt(a, "readable_date"),
        contact_name: opt(a, "contact_name"),
        text: String::new(),
        blobs: Vec::new(),
        failed_blobs: Vec::new(),
    }
}

type Attrs = HashMap<String, String>;

fn attrs(e: &BytesStart) -> Result<Attrs> {
    let mut map = HashMap::new();
    for attr in e.attributes() {
        let attr = attr.context("parse attribute")?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let val = datalib_etl::xml::attr_value(&attr)
            .context("unescape attribute")?
            .into_owned();
        map.insert(key, val);
    }
    Ok(map)
}

fn opt(a: &Attrs, key: &str) -> Option<String> {
    a.get(key)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(str::to_string)
}

fn int(a: &Attrs, key: &str) -> Option<i64> {
    opt(a, key).and_then(|s| s.trim().parse::<i64>().ok())
}

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
  <sms protocol="0" address="+17783176760" date="1778277198761" type="1" body="Hello, right back at you!" read="0" date_sent="1778277198000" readable_date="May 8, 2026 2:53:18 p.m." contact_name="(Unknown)" />
  <sms protocol="0" address="+12262121542" date="1778277388135" type="1" body="&lt;#&gt; code 763&#10;line two" date_sent="0" readable_date="x" contact_name="null" />
</smses>"#;

    #[test]
    fn detect_root_kinds() {
        assert_eq!(detect_root(SMSES).unwrap(), RootKind::Smses);
        assert_eq!(
            detect_root("<calls count=\"0\"></calls>").unwrap(),
            RootKind::Calls
        );
        assert_eq!(detect_root("<other/>").unwrap(), RootKind::Other);
    }

    #[test]
    fn a_file_with_no_element_is_not_an_export_of_nothing() {
        assert!(detect_root("").is_err());
        assert!(detect_root("<?xml version='1.0' ?>\n").is_err());
        assert!(detect_root("\u{1}\u{2} not xml").is_err());
    }

    #[test]
    fn a_backup_cut_off_part_way_does_not_parse() {
        let cut = &SMSES[..SMSES.find("</smses>").unwrap()];
        assert!(parse_smses(cut).is_err());
        assert!(parse_calls("<calls count=\"1\"><call number=\"1\" />").is_err());
        assert_eq!(parse_smses("<smses count=\"0\"/>").unwrap().0.len(), 0);
        assert!(parse_calls("<calls count=\"0\"></calls>")
            .unwrap()
            .is_empty());
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
        // `read="0"` is kept; a backup that says nothing stays unknown.
        assert_eq!(sms[0].read, Some(false));
        assert_eq!(sms[1].read, None);
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

    /// A decoded part after one that would not decode keeps the name it
    /// had when the bad part was skipped, so its stored edge still matches.
    #[test]
    fn a_part_after_one_that_will_not_decode_keeps_its_name() {
        let gif = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        let xml = format!(
            r#"<smses count="1">
  <mms date="1781811656000" msg_box="1" address="+17015550101" m_id="NCC-1701-D">
    <parts>
      <part seq="0" ct="image/gif" data="%%% not base64 %%%" />
      <part seq="1" ct="image/gif" data="{gif}" />
    </parts>
  </mms>
</smses>"#
        );
        let (_, mms) = parse_smses(&xml).unwrap();
        assert_eq!(mms[0].blobs[0].name, "part0.gif");
        assert_eq!(mms[0].failed_blobs[0].0, "undecoded0");
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
