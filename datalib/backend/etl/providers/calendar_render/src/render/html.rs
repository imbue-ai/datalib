//! A calendar description written as HTML, as the text a person reads.
//! Google's API hands every description as the HTML its editor wrote, and
//! an event made in Google keeps that HTML in its iCalendar `DESCRIPTION`
//! wherever it is copied (a Fastmail calendar serves such events over
//! CalDAV), although the field is plain text there.

/// The tags an event editor writes. A description holding one is HTML;
/// a plain-text one that merely has a `<` in it is left as written.
const EDITOR_TAGS: &[&str] = &[
    "a", "b", "br", "div", "em", "font", "html", "i", "li", "ol", "p", "span", "strong", "u", "ul",
];

pub fn holds_html(text: &str) -> bool {
    text.match_indices('<').any(|(i, _)| {
        let tag = text[i + 1..].trim_start_matches('/');
        let name_len = tag
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tag.len());
        let after = tag[name_len..].chars().next();
        matches!(after, Some('>' | '/' | ' ' | '\t' | '\n'))
            && EDITOR_TAGS.contains(&tag[..name_len].to_ascii_lowercase().as_str())
    })
}

/// A description written as HTML, as text. Line breaks and list items
/// become lines; a link keeps its address where the text did not already
/// show it; every other tag goes.
pub fn html_to_text(html: &str) -> String {
    if !html.contains('<') && !html.contains('&') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut href: Option<String> = None;
    let mut link_text = String::new();
    while let Some(i) = rest.find('<') {
        let text = decode_entities(&rest[..i]);
        out.push_str(&text);
        if href.is_some() {
            link_text.push_str(&text);
        }
        let Some(j) = rest[i..].find('>') else {
            out.push_str(&decode_entities(&rest[i..]));
            rest = "";
            break;
        };
        let tag = &rest[i + 1..i + j];
        rest = &rest[i + j + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');
        match (name.as_str(), closing) {
            ("br", _) | ("p" | "div" | "tr" | "ul" | "ol", true) => out.push('\n'),
            ("li", false) => out.push_str("\n- "),
            ("a", false) => {
                href = attr(tag, "href");
                link_text.clear();
            }
            ("a", true) => {
                if let Some(h) = href.take() {
                    if !link_text.contains(h.trim_start_matches("mailto:")) {
                        out.push_str(&format!(" ({h})"));
                    }
                }
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let mut joined = lines.join("\n");
    while joined.contains("\n\n\n") {
        joined = joined.replace("\n\n\n", "\n\n");
    }
    joined.trim().to_string()
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let (quote, body) = match rest.chars().next()? {
        q @ ('"' | '\'') => (q, &rest[1..]),
        _ => (' ', rest),
    };
    let end = body.find(quote).unwrap_or(body.len());
    Some(decode_entities(&body[..end]))
}

/// A no-break space reads as a space in a description's text.
fn decode_entities(s: &str) -> String {
    datalib_html_entities::decode(s).replace('\u{a0}', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_lose_their_markup_but_not_their_lines_or_links() {
        assert_eq!(
            html_to_text("Agenda:<br><ul><li>Saucer <b>separation</b></li><li>Drill</li></ul><a href=\"https://memory-alpha.test/x\">notes</a> &amp; more"),
            "Agenda:\n\n- Saucer separation\n- Drill\nnotes (https://memory-alpha.test/x) & more"
        );
        assert_eq!(
            html_to_text("<a href=\"https://meet.test/a\">https://meet.test/a</a>"),
            "https://meet.test/a"
        );
        assert_eq!(html_to_text("plain\ntext"), "plain\ntext");
    }

    /// A Google-made event that Fastmail serves over CalDAV keeps its HTML
    /// in `DESCRIPTION`, and its page showed the tags, escaped, as text.
    #[test]
    fn html_is_told_from_plain_text_that_has_a_bracket() {
        assert!(holds_html(
            "Host: <a href=\"mailto:picard@enterprise.test\">Picard</a>"
        ));
        assert!(holds_html("Agenda<br>Drill"));
        assert!(holds_html("<p>Agenda</p>"));
        assert!(holds_html("line<BR/>break"));
        assert!(!holds_html("Warp factor < 9 and > 5"));
        assert!(!holds_html("Reply to <picard@enterprise.test>"));
        assert!(!holds_html("Use <tab> to indent"));
        assert!(!holds_html("plain text"));
    }
}
