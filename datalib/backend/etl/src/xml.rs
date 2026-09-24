//! The one quick-xml idiom every XML-reading provider shares: an
//! element's text can arrive in three kinds of event — plain text, a
//! CDATA section, and (since quick-xml 0.38) an entity or character
//! reference (`&amp;`, `&#8217;`) between text events — and the parser
//! appends each to the text it sits in.

use std::borrow::Cow;

use quick_xml::escape::{resolve_html5_entity, resolve_xml_entity, unescape, EscapeError};
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesRef, Event};

/// An attribute's value with its references resolved and nothing else
/// changed — what `Attribute::unescape_value` did before quick-xml
/// deprecated it for `normalized_value`, which also folds every literal
/// newline and tab in the value into a space. An SMS body is an
/// attribute here, and its line breaks are content.
pub fn attr_value<'a>(attr: &'a Attribute<'a>) -> Result<Cow<'a, str>, AttrError> {
    let raw = std::str::from_utf8(&attr.value).map_err(|_| AttrError::NotUtf8)?;
    unescape(raw).map_err(AttrError::Escape)
}

#[derive(Debug)]
pub enum AttrError {
    NotUtf8,
    Escape(EscapeError),
}

impl std::fmt::Display for AttrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttrError::NotUtf8 => write!(f, "attribute value is not UTF-8"),
            AttrError::Escape(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttrError {}

/// The text an event adds to the element it sits in, or `None` for an
/// event that carries none. CDATA is text too, verbatim: Fastmail's
/// WebDAV wraps every property value in it.
pub fn text_of<'a>(event: &Event<'a>, html: bool) -> Option<Cow<'a, str>> {
    match event {
        Event::Text(t) => Some(t.decode().unwrap_or_default()),
        Event::CData(t) => Some(t.decode().unwrap_or_default()),
        Event::GeneralRef(r) => Some(Cow::Owned(reference_text(r, html))),
        _ => None,
    }
}

/// What the reference stands for. An unknown name comes back verbatim
/// (`&name;`) rather than vanishing: the source said it, so the
/// rendered text keeps it.
pub fn reference_text(r: &BytesRef<'_>, html: bool) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    let name = match r.decode() {
        Ok(name) => name,
        Err(_) => return String::new(),
    };
    let resolved = if html {
        resolve_html5_entity(&name)
    } else {
        resolve_xml_entity(&name)
    };
    match resolved {
        Some(text) => text.to_string(),
        None => format!("&{name};"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    fn refs(doc: &str, html: bool) -> Vec<String> {
        let mut reader = Reader::from_str(doc);
        let mut out = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::GeneralRef(r) => out.push(reference_text(&r, html)),
                Event::Eof => break,
                _ => {}
            }
        }
        out
    }

    /// The value keeps its literal newlines: `normalized_value` would
    /// fold them, and an SMS body arrives as an attribute.
    #[test]
    fn attr_value_resolves_references_and_keeps_line_breaks() {
        let mut reader = Reader::from_str("<sms body=\"a &amp; b&#10;c\nd\"/>");
        let Event::Empty(e) = reader.read_event().unwrap() else {
            panic!("expected the element");
        };
        let attr = e.attributes().next().unwrap().unwrap();
        assert_eq!(attr_value(&attr).unwrap(), "a & b\nc\nd");
    }

    #[test]
    fn text_of_joins_text_cdata_and_references() {
        let mut reader = Reader::from_str("<a>x &amp; <![CDATA[<y> &amp;]]>\r\n</a>");
        let mut text = String::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Eof => break,
                event => text.push_str(&text_of(&event, false).unwrap_or_default()),
            }
        }
        assert_eq!(text, "x & <y> &amp;\r\n");
    }

    #[test]
    fn resolves_predefined_numeric_and_html_references() {
        assert_eq!(refs("a&amp;b&#8217;c&#x41;", false), ["&", "\u{2019}", "A"]);
        assert_eq!(refs("&nbsp;", true), ["\u{a0}"]);
        // XML has no `&nbsp;`; the text keeps what the source said.
        assert_eq!(refs("&nbsp;", false), ["&nbsp;"]);
    }
}
