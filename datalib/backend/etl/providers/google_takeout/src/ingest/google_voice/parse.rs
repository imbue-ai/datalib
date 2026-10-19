//! Parsers for the Google Voice takeout HTML.

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
    /// A `rel="tag"` link's fragment (`…/voice#placed`), Google's label
    /// for what the record is.
    fn from_tag(fragment: &str) -> Option<Self> {
        match fragment {
            "voicemail" => Some(Self::Voicemail),
            "missed" => Some(Self::Missed),
            "placed" => Some(Self::Placed),
            "received" => Some(Self::Received),
            "recorded" => Some(Self::Recorded),
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

/// What a Voice file is, read off its markup where its name does not say:
/// Google leaves the type out of some names (`<who> - <stamp>.html`) and
/// cuts it short in a long one (`<who> - Pla(3).html`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// An `hChatLog`: a text thread or a group conversation.
    Thread,
    /// An `haudio`: a call or voicemail, of the kind its tags name, or
    /// its title's first word.
    Call(CallKind),
}

pub fn record_of(html: &str) -> Option<Record> {
    let pre = preprocess(html);
    let mut r = reader(&pre);
    let mut buf = Vec::new();
    let (mut thread, mut call) = (false, false);
    let mut tagged: Option<CallKind> = None;
    let (mut in_title, mut title) = (false, String::new());
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                "div" if has_class(&e, "hChatLog") => thread = true,
                "div" if has_class(&e, "haudio") => call = true,
                "a" if attr(&e, "rel").as_deref() == Some("tag") => {
                    let href = attr(&e, "href").unwrap_or_default();
                    let fragment = href.rsplit_once('#').map_or("", |(_, f)| f);
                    tagged = tagged.or(CallKind::from_tag(fragment));
                }
                "title" => in_title = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_title => {
                title.push_str(&t);
            }
            Ok(Event::End(e)) if e.name().as_ref() == "title" => in_title = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    if thread {
        return Some(Record::Thread);
    }
    let titled = title
        .split_whitespace()
        .next()
        .and_then(CallKind::from_type_token);
    call.then(|| tagged.or(titled)).flatten().map(Record::Call)
}

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

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| {
            datalib_etl::xml::attr_value(&a)
                .ok()
                .map(|c| c.into_owned())
        })
}

fn has_class(e: &BytesStart, want: &str) -> bool {
    attr(e, "class")
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
                    "div" => {
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
                    "abbr" => {
                        if has_class(&e, "dt") {
                            if let (Some(m), Some(t)) = (cur.as_mut(), attr(&e, "title")) {
                                m.dt = t;
                            }
                        } else if has_class(&e, "fn") {
                            capture_fn = true;
                        }
                    }
                    "a" if has_class(&e, "tel") => {
                        in_tel = true;
                        if let (Some(m), Some(h)) = (cur.as_mut(), attr(&e, "href")) {
                            m.sender.tel = tel_from_href(&h);
                        }
                    }
                    "span" if has_class(&e, "fn") => capture_fn = true,
                    "q" => {
                        in_q = true;
                        q_text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == "img" => {
                if let (Some(m), Some(src)) = (cur.as_mut(), attr(&e, "src")) {
                    m.attachments.push(src);
                }
            }
            // A reference (`&amp;`, `&#8217;`) is text too — same handling.
            Ok(event @ (Event::Text(_) | Event::GeneralRef(_))) => {
                let txt = match event {
                    Event::Text(t) => t.to_string(),
                    Event::GeneralRef(r) => datalib_etl::xml::reference_text(&r, true),
                    _ => unreachable!(),
                };
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
                    "q" => {
                        in_q = false;
                        if let Some(m) = cur.as_mut() {
                            m.body = q_text.trim().to_string();
                        }
                    }
                    "a" => {
                        in_tel = false;
                        capture_fn = false;
                    }
                    "span" | "abbr" => capture_fn = false,
                    "div" => {
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
                    "a" if has_class(&e, "tel") => {
                        in_tel = true;
                        if let Some(h) = attr(&e, "href") {
                            ev.party.tel = tel_from_href(&h);
                        }
                    }
                    // `<a rel="enclosure" href="…mp3">` — audio fallback.
                    "a" if attr(&e, "rel").as_deref() == Some("enclosure") => {
                        if let Some(h) = attr(&e, "href") {
                            ev.audio_src.get_or_insert(h);
                        }
                    }
                    "span" if has_class(&e, "fn") => capture_fn = true,
                    "span" if has_class(&e, "full-text") => {
                        in_full_text = true;
                        full_text.clear();
                    }
                    "abbr" if has_class(&e, "published") => {
                        if let Some(t) = attr(&e, "title") {
                            ev.published = t;
                        }
                    }
                    "abbr" if has_class(&e, "duration") => {
                        ev.duration = attr(&e, "title");
                    }
                    "audio" => {
                        if let Some(src) = attr(&e, "src") {
                            ev.audio_src = Some(src);
                        }
                    }
                    _ => {}
                }
            }
            // A reference (`&amp;`, `&#8217;`) is text too — same handling.
            Ok(event @ (Event::Text(_) | Event::GeneralRef(_))) => {
                let txt = match event {
                    Event::Text(t) => t.to_string(),
                    Event::GeneralRef(r) => datalib_etl::xml::reference_text(&r, true),
                    _ => unreachable!(),
                };
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
                "a" => {
                    in_tel = false;
                    capture_fn = false;
                }
                "span" => {
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
                "tr" => {
                    in_row = true;
                    cur_row = Vec::new();
                }
                "th" => {
                    in_th = true;
                    cell.clear();
                }
                "td" => {
                    in_td = true;
                    cell.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_th || in_td => {
                cell.push_str(&t);
            }
            Ok(Event::GeneralRef(r)) if in_th || in_td => {
                cell.push_str(&datalib_etl::xml::reference_text(&r, true));
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "th" => {
                    in_th = false;
                    headers.push(cell.trim().to_string());
                }
                "td" => {
                    in_td = false;
                    cur_row.push(cell.trim().to_string());
                }
                "tr" => {
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
<div class="message"><abbr class="dt" title="2364-03-01T09:00:00.742-08:00">Mar 1</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+12025550102"><span class="fn">William Riker</span></a></cite>:
<q>Ready when you are, Captain&#39;s orders</q></div>
<div class="message"><abbr class="dt" title="2364-03-01T09:14:59.672-08:00">Mar 1</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+12025550100"><abbr class="fn" title="">Me</abbr></a></cite>:
<q>Engage!<br>Multi-line</q></div>
<div class="message"><abbr class="dt" title="2364-03-02T09:06:01.024-08:00">x</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+12025550199"><span class="fn"></span></a></cite>:
<q>MMS Received</q>
<div><img src="img-ref-no-ext" alt="Image MMS Attachment" /></div></div>
</div></body></html>"#;

    #[test]
    fn parses_text_thread() {
        let m = parse_chat_log(TEXT);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].sender.tel.as_deref(), Some("+12025550102"));
        assert_eq!(m[0].sender.name.as_deref(), Some("William Riker"));
        assert!(!m[0].is_me);
        assert_eq!(m[0].body, "Ready when you are, Captain's orders");
        // sent-by-me + <br> → newline
        assert!(m[1].is_me);
        assert_eq!(m[1].sender.tel.as_deref(), Some("+12025550100"));
        assert_eq!(m[1].body, "Engage!\nMulti-line");
        // attachment ref captured
        assert_eq!(m[2].attachments, vec!["img-ref-no-ext".to_string()]);
        assert_eq!(m[2].body, "MMS Received");
    }

    const VM: &str = r#"<html><body><div class="haudio"><span class="fn">Voicemail from
</span>
<div class="contributor vcard">Voicemail from
<a class="tel" href="tel:+15551234567"><span class="fn">Jean-Luc Picard</span></a></div>
<abbr class="published" title="2364-02-18T16:10:05.000-08:00">Feb 18</abbr>
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
        assert_eq!(e.published, "2364-02-18T16:10:05.000-08:00");
        assert_eq!(e.transcript.as_deref(), Some("Make it so."));
        assert_eq!(e.audio_src.as_deref(), Some("vm.mp3"));
        assert_eq!(e.duration.as_deref(), Some("PT13S"));
    }

    const MISSED: &str = r#"<html><body><div class="haudio"><span class="fn">Missed call from
</span>
<div class="contributor vcard">Missed call from
<a class="tel" href="tel:+15559998888"><span class="fn">Spammer</span></a></div>
<abbr class="published" title="2364-03-06T09:50:34.000-08:00">Mar 6</abbr>
</div></body></html>"#;

    #[test]
    fn parses_missed_call() {
        let e = parse_haudio(MISSED);
        assert_eq!(e.party.tel.as_deref(), Some("+15559998888"));
        assert_eq!(e.party.name.as_deref(), Some("Spammer"));
        assert_eq!(e.published, "2364-03-06T09:50:34.000-08:00");
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

    /// A placed call Google named `<who> - <stamp>.html`, and one whose
    /// long name it cut to `<who> - Pla(3).html`, read as text threads
    /// holding no message and failed.
    #[test]
    fn a_record_is_read_off_its_markup() {
        let call = |tag: &str, title: &str| {
            format!(
                "<html><head><title>{title}</title></head><body><div class=\"haudio\">\
                 <abbr class=\"published\" title=\"2364-03-05T08:00:00.000-08:00\">Mar 5</abbr>\
                 <div class=\"tags\">Labels: {tag}</div></div></body></html>"
            )
        };
        let placed = call(
            "<a rel=\"tag\" href=\"http://www.google.com/voice#placed\">Placed</a>",
            "Placed call to Geordi La Forge",
        );
        assert_eq!(record_of(&placed), Some(Record::Call(CallKind::Placed)));
        let voicemail = call(
            "<a rel=\"tag\" href=\"http://www.google.com/voice#inbox\">Inbox</a>, \
             <a rel=\"tag\" href=\"http://www.google.com/voice#voicemail\">Voicemail</a>",
            "Voicemail from Worf",
        );
        assert_eq!(
            record_of(&voicemail),
            Some(Record::Call(CallKind::Voicemail))
        );
        let untagged = call("", "Received call from Guinan");
        assert_eq!(record_of(&untagged), Some(Record::Call(CallKind::Received)));
        assert_eq!(
            record_of("<html><body><div class=\"hChatLog hfeed\"></div></body></html>"),
            Some(Record::Thread)
        );
        assert_eq!(
            record_of("<html><body><p>Tea, Earl Grey</p></body></html>"),
            None
        );
    }

    #[test]
    fn tel_href() {
        assert_eq!(tel_from_href("tel:+1650"), Some("+1650".to_string()));
        assert_eq!(tel_from_href("tel:"), None);
    }
}
