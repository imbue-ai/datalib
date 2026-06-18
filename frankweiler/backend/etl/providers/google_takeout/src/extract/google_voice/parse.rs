//! Parsers for the Google Voice takeout HTML.
//!
//! Google Voice exports two XHTML shapes (both under `Voice/Calls/` and
//! `Voice/Spam/`), plus a single `Bills.html` table:
//!
//!   * **`hChatLog`** (Text / Group Conversation) — a `<div class="hChatLog">`
//!     holding N `<div class="message">` blocks, each with an
//!     `<abbr class="dt" title="<rfc3339>">`, a
//!     `<cite class="sender vcard"><a class="tel" href="tel:+…"><span class="fn">Name</span></a>`
//!     (sent messages use `fn="Me"`), a `<q>body</q>`, and optional
//!     `<img src="…">` attachment refs.
//!   * **`haudio`** (Voicemail / Missed / Placed / Received / Recorded) —
//!     a `<div class="contributor vcard">` for the other party, an
//!     `<abbr class="published" title=…>`, and — for voicemails — a
//!     `<span class="full-text">transcript</span>`, `<audio src="…mp3">`,
//!     and `<abbr class="duration" title="PT…S">`.
//!
//! The files declare XHTML 1.0 Strict but use bare `<br>` in message
//! bodies, which is not well-formed XML; we pre-pass `<br>`→newline so
//! `quick-xml` (XML pull mode) parses cleanly. Entity decoding (`&#39;`,
//! `&#8239;`, …) is handled by quick-xml's `unescape`.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// A phone number + display name for one party.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Party {
    pub tel: Option<String>,
    pub name: Option<String>,
}

/// One text message parsed out of an `hChatLog` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    /// Raw `title=` timestamp (RFC3339 with offset).
    pub dt: String,
    pub sender: Party,
    /// Sent by the account owner (`fn="Me"`).
    pub is_me: bool,
    pub body: String,
    /// Attachment `src` refs (resolved to sibling files by the caller).
    pub attachments: Vec<String>,
}

/// The non-text record kinds, taken from the takeout filename's Type token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Voicemail,
    Missed,
    Placed,
    Received,
    Recorded,
}

impl CallKind {
    pub fn from_type_token(s: &str) -> Option<Self> {
        match s {
            "Voicemail" => Some(Self::Voicemail),
            "Missed" => Some(Self::Missed),
            "Placed" => Some(Self::Placed),
            "Received" => Some(Self::Received),
            "Recorded" => Some(Self::Recorded),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Voicemail => "voicemail",
            Self::Missed => "missed",
            Self::Placed => "placed",
            Self::Received => "received",
            Self::Recorded => "recorded",
        }
    }
}

/// One call/voicemail event parsed out of an `haudio` file. `kind` is
/// assigned by the caller from the filename.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedEvent {
    pub party: Party,
    /// Raw `title=` timestamp (RFC3339 with offset).
    pub published: String,
    pub transcript: Option<String>,
    /// `src` of the `<audio>` element (sibling mp3), if present.
    pub audio_src: Option<String>,
    /// ISO-8601 duration (`PT13S`), if present.
    pub duration: Option<String>,
}

/// `tel:+16506463903` → `+16506463903`; empty / `tel:` → `None`.
pub fn tel_from_href(href: &str) -> Option<String> {
    let t = href.trim().strip_prefix("tel:").unwrap_or(href).trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Bare `<br>` (not self-closed) breaks XML parsing; turn every `<br>`
/// variant into a newline before parsing. GV only uses `<br>` as a soft
/// line break inside message bodies, so this is lossless.
fn preprocess(html: &str) -> String {
    html.replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("<br>", "\n")
}

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| a.unescape_value().ok().map(|c| c.into_owned()))
}

fn has_class(e: &BytesStart, want: &str) -> bool {
    attr(e, b"class")
        .map(|c| c.split_whitespace().any(|t| t == want))
        .unwrap_or(false)
}

fn reader(pre: &str) -> Reader<&[u8]> {
    let mut r = Reader::from_str(pre);
    r.config_mut().check_end_names = false;
    r
}

/// Parse an `hChatLog` (Text / Group Conversation) file into its messages,
/// in document order.
pub fn parse_chat_log(html: &str) -> Vec<ParsedMessage> {
    let pre = preprocess(html);
    let mut r = reader(&pre);
    let mut buf = Vec::new();
    let mut msgs = Vec::new();

    let mut cur: Option<ParsedMessage> = None;
    let mut div_depth = 0usize;
    let mut msg_div_depth: Option<usize> = None;
    let mut in_q = false;
    let mut q_text = String::new();
    let mut in_tel = false; // inside the sender's <a class="tel">
    let mut capture_fn = false; // next text node is a display name

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let owned = e.name();
                match owned.as_ref() {
                    b"div" => {
                        div_depth += 1;
                        if has_class(&e, "message") {
                            cur = Some(ParsedMessage {
                                dt: String::new(),
                                sender: Party::default(),
                                is_me: false,
                                body: String::new(),
                                attachments: Vec::new(),
                            });
                            msg_div_depth = Some(div_depth);
                        }
                    }
                    b"abbr" => {
                        if has_class(&e, "dt") {
                            if let (Some(m), Some(t)) = (cur.as_mut(), attr(&e, b"title")) {
                                m.dt = t;
                            }
                        } else if has_class(&e, "fn") {
                            capture_fn = true;
                        }
                    }
                    b"a" if has_class(&e, "tel") => {
                        in_tel = true;
                        if let (Some(m), Some(h)) = (cur.as_mut(), attr(&e, b"href")) {
                            m.sender.tel = tel_from_href(&h);
                        }
                    }
                    b"span" if has_class(&e, "fn") => capture_fn = true,
                    b"q" => {
                        in_q = true;
                        q_text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"img" => {
                if let (Some(m), Some(src)) = (cur.as_mut(), attr(&e, b"src")) {
                    m.attachments.push(src);
                }
            }
            Ok(Event::Text(t)) => {
                let txt = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                if in_q {
                    q_text.push_str(&txt);
                } else if capture_fn && in_tel {
                    let trimmed = txt.trim();
                    if !trimmed.is_empty() {
                        if let Some(m) = cur.as_mut() {
                            if trimmed.eq_ignore_ascii_case("Me") {
                                m.is_me = true;
                            } else {
                                m.sender.name = Some(trimmed.to_string());
                            }
                        }
                        capture_fn = false;
                    }
                }
            }
            Ok(Event::End(e)) => {
                let owned = e.name();
                match owned.as_ref() {
                    b"q" => {
                        in_q = false;
                        if let Some(m) = cur.as_mut() {
                            m.body = q_text.trim().to_string();
                        }
                    }
                    b"a" => {
                        in_tel = false;
                        capture_fn = false;
                    }
                    b"span" | b"abbr" => capture_fn = false,
                    b"div" => {
                        if Some(div_depth) == msg_div_depth {
                            if let Some(m) = cur.take() {
                                msgs.push(m);
                            }
                            msg_div_depth = None;
                        }
                        div_depth = div_depth.saturating_sub(1);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    msgs
}

/// Parse an `haudio` (Voicemail / call) file into its single event.
pub fn parse_haudio(html: &str) -> ParsedEvent {
    let pre = preprocess(html);
    let mut r = reader(&pre);
    let mut buf = Vec::new();
    let mut ev = ParsedEvent::default();

    let mut in_tel = false;
    let mut capture_fn = false;
    let mut in_full_text = false;
    let mut full_text = String::new();

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let owned = e.name();
                match owned.as_ref() {
                    b"a" if has_class(&e, "tel") => {
                        in_tel = true;
                        if let Some(h) = attr(&e, b"href") {
                            ev.party.tel = tel_from_href(&h);
                        }
                    }
                    // `<a rel="enclosure" href="…mp3">` — audio fallback.
                    b"a" if attr(&e, b"rel").as_deref() == Some("enclosure") => {
                        if let Some(h) = attr(&e, b"href") {
                            ev.audio_src.get_or_insert(h);
                        }
                    }
                    b"span" if has_class(&e, "fn") => capture_fn = true,
                    b"span" if has_class(&e, "full-text") => {
                        in_full_text = true;
                        full_text.clear();
                    }
                    b"abbr" if has_class(&e, "published") => {
                        if let Some(t) = attr(&e, b"title") {
                            ev.published = t;
                        }
                    }
                    b"abbr" if has_class(&e, "duration") => {
                        ev.duration = attr(&e, b"title");
                    }
                    b"audio" => {
                        if let Some(src) = attr(&e, b"src") {
                            ev.audio_src = Some(src);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                let txt = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                if in_full_text {
                    full_text.push_str(&txt);
                } else if capture_fn && in_tel {
                    let trimmed = txt.trim();
                    if !trimmed.is_empty() {
                        ev.party.name = Some(trimmed.to_string());
                        capture_fn = false;
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"a" => {
                    in_tel = false;
                    capture_fn = false;
                }
                b"span" => {
                    capture_fn = false;
                    if in_full_text {
                        in_full_text = false;
                        let t = full_text.trim();
                        if !t.is_empty() {
                            ev.transcript = Some(t.to_string());
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    ev
}

/// Parse `Bills.html`'s single table into (headers, rows-of-cells).
pub fn parse_bills(html: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let pre = preprocess(html);
    let mut r = reader(&pre);
    let mut buf = Vec::new();
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_th = false;
    let mut in_td = false;
    let mut in_row = false;

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"tr" => {
                    in_row = true;
                    cur_row = Vec::new();
                }
                b"th" => {
                    in_th = true;
                    cell.clear();
                }
                b"td" => {
                    in_td = true;
                    cell.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_th || in_td => {
                cell.push_str(&t.unescape().map(|c| c.into_owned()).unwrap_or_default());
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"th" => {
                    in_th = false;
                    headers.push(cell.trim().to_string());
                }
                b"td" => {
                    in_td = false;
                    cur_row.push(cell.trim().to_string());
                }
                b"tr" => {
                    in_row = false;
                    if !cur_row.is_empty() {
                        rows.push(std::mem::take(&mut cur_row));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        let _ = in_row;
        buf.clear();
    }
    (headers, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"<html><body><div class="hChatLog hfeed">
<div class="message"><abbr class="dt" title="2019-08-01T14:49:00.742-07:00">Aug 1</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+14102127741"><span class="fn">Wes Blackwell</span></a></cite>:
<q>Hope you&#39;re well brother</q></div>
<div class="message"><abbr class="dt" title="2019-08-01T15:03:59.672-07:00">Aug 1</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+16506463903"><abbr class="fn" title="">Me</abbr></a></cite>:
<q>Wow!<br>Multi-line</q></div>
<div class="message"><abbr class="dt" title="2024-02-02T09:06:01.024-08:00">x</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+12027687727"><span class="fn"></span></a></cite>:
<q>MMS Received</q>
<div><img src="img-ref-no-ext" alt="Image MMS Attachment" /></div></div>
</div></body></html>"#;

    #[test]
    fn parses_text_thread() {
        let m = parse_chat_log(TEXT);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].sender.tel.as_deref(), Some("+14102127741"));
        assert_eq!(m[0].sender.name.as_deref(), Some("Wes Blackwell"));
        assert!(!m[0].is_me);
        assert_eq!(m[0].body, "Hope you're well brother");
        // sent-by-me + <br> → newline
        assert!(m[1].is_me);
        assert_eq!(m[1].sender.tel.as_deref(), Some("+16506463903"));
        assert_eq!(m[1].body, "Wow!\nMulti-line");
        // attachment ref captured
        assert_eq!(m[2].attachments, vec!["img-ref-no-ext".to_string()]);
        assert_eq!(m[2].body, "MMS Received");
    }

    const VM: &str = r#"<html><body><div class="haudio"><span class="fn">Voicemail from
</span>
<div class="contributor vcard">Voicemail from
<a class="tel" href="tel:+15551234567"><span class="fn">Jean-Luc Picard</span></a></div>
<abbr class="published" title="2010-02-18T16:10:05.000-08:00">Feb 18</abbr>
Transcript:
<span class="description"><span class="full-text">Make it so.</span></span>
<audio controls="controls" src="vm.mp3"><a rel="enclosure" href="vm.mp3">Audio</a></audio>
<abbr class="duration" title="PT13S">(00:00:13)</abbr>
</div></body></html>"#;

    #[test]
    fn parses_voicemail() {
        let e = parse_haudio(VM);
        assert_eq!(e.party.tel.as_deref(), Some("+15551234567"));
        assert_eq!(e.party.name.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(e.published, "2010-02-18T16:10:05.000-08:00");
        assert_eq!(e.transcript.as_deref(), Some("Make it so."));
        assert_eq!(e.audio_src.as_deref(), Some("vm.mp3"));
        assert_eq!(e.duration.as_deref(), Some("PT13S"));
    }

    const MISSED: &str = r#"<html><body><div class="haudio"><span class="fn">Missed call from
</span>
<div class="contributor vcard">Missed call from
<a class="tel" href="tel:+15559998888"><span class="fn">Spammer</span></a></div>
<abbr class="published" title="2009-03-06T09:50:34.000-08:00">Mar 6</abbr>
</div></body></html>"#;

    #[test]
    fn parses_missed_call() {
        let e = parse_haudio(MISSED);
        assert_eq!(e.party.tel.as_deref(), Some("+15559998888"));
        assert_eq!(e.party.name.as_deref(), Some("Spammer"));
        assert_eq!(e.published, "2009-03-06T09:50:34.000-08:00");
        assert!(e.transcript.is_none());
        assert!(e.audio_src.is_none());
    }

    #[test]
    fn parses_bills() {
        let html = r#"<table><tr><th>Date/Time</th><th>Type</th><th>Money</th></tr>
<tr><td class="fn">Free credit</td><td>Credit</td><td>$0.50</td></tr>
<tr><td class="fn">Phone call</td><td>Call</td><td>-$0.02</td></tr></table>"#;
        let (headers, rows) = parse_bills(html);
        assert_eq!(headers, vec!["Date/Time", "Type", "Money"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["Free credit", "Credit", "$0.50"]);
    }

    #[test]
    fn tel_href() {
        assert_eq!(tel_from_href("tel:+1650"), Some("+1650".to_string()));
        assert_eq!(tel_from_href("tel:"), None);
    }
}
