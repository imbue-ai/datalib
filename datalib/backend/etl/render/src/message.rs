//! The one message header every renderer writes.
//!
//! A rendered document is a stack of sections, and a section that has
//! an author and a time says so the same way wherever it came from — a
//! Slack message, a Signal SMS, a PR review comment. This is that line.

use datalib_handle::Handle;
use datalib_time::IsoOffsetTimestamp;

use crate::html::{escape_attr, escape_md_inline, md_link_dest};

/// A chip: a markdown link naming a person, which datalib draws as a
/// chip and every other viewer shows as a link with a tooltip
/// (`docs/dev/plans/chips.md`). The text is what the source showed, the
/// href is the handle as a URI, and the title is the static hover — the
/// name and the identifier — which datalib's live hover card replaces.
pub fn chip_link(shown: &str, handle: &Handle) -> String {
    let shown = shown.trim();
    let text = if shown.is_empty() {
        handle.value()
    } else {
        shown
    };
    link_with_title(text, &handle.to_uri(), &handle.describe(shown))
}

/// A chip naming anything else the app resolves — a group, a step — by
/// its URI (`datalib:group/slack`). The same link shape as a person's:
/// the text is the name to show until it resolves, the title the hover.
pub fn entity_link(text: &str, uri: &str, title: &str) -> String {
    link_with_title(text, uri, title)
}

fn link_with_title(text: &str, href: &str, title: &str) -> String {
    format!(
        "[{}]({} \"{}\")",
        escape_md_inline(text),
        md_link_dest(href),
        md_link_title(title),
    )
}

/// Text inside a link's double-quoted title: the quote and the
/// backslash escaped, a line break a space. markdown-it HTML-escapes the
/// attribute it makes of it.
fn md_link_title(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// One message header, rendered as a `## ` line whose parts are tagged
/// for the frontend.
#[derive(Debug, Clone, Default)]
pub struct MessageHeader<'a> {
    /// Who said it, as a reader should see it ("Me", "Will Riker",
    /// "@jlpicard"). Plain text: the span sits on a markdown line, so it
    /// is escaped as markdown and HTML both.
    pub author: &'a str,
    /// Who said it, as an identifier. With one, the author is a chip
    /// link ([`chip_link`]) that the UI resolves to a contact; without,
    /// a plain `msg-author` span. The text is `author` either way.
    pub handle: Option<&'a Handle>,
    /// When, in Unix milliseconds. `None` renders as "(no timestamp)"
    /// rather than as a stand-in instant.
    pub date_ms: Option<i64>,
    /// Public URL for this individual message (a Slack permalink, a
    /// GitHub comment anchor). Rendered as a trailing `↗`.
    pub source_url: Option<&'a str>,
}

impl MessageHeader<'_> {
    /// The header line, without a trailing newline.
    ///
    /// **It is a real `##`, and that is load-bearing.** qmd cuts a
    /// chunk at the best break point near its size limit and scores an
    /// `h2` far above the blank line it would otherwise settle for, so
    /// the heading is what makes every message start a chunk boundary
    /// it prefers. The frontend styles it down to a one-line
    /// "name, then a small grey time"; it is not a heading on screen.
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(160);
        s.push_str("## ");
        // An empty author is an event upstream attributed to nobody (a
        // room-creation notice, say). Say nothing rather than drawing an
        // empty name.
        if !self.author.is_empty() {
            match self.handle {
                Some(h) => s.push_str(&chip_link(self.author, h)),
                None => s.push_str(&format!(
                    "<span class=\"msg-author\">{}</span>",
                    escape_md_inline(self.author),
                )),
            }
            s.push(' ');
        }
        s.push_str(&timestamp_html(self.date_ms));
        if let Some(url) = self.source_url {
            s.push_str(&format!(
                " <a class=\"source-link\" href=\"{url}\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a>",
                url = escape_attr(url),
            ));
        }
        s
    }
}

/// A timestamp a reader can hover for the full instant, or a plain span
/// when there is no instant to state.
///
/// The short form on screen is deliberately absolute rather than
/// relative ("Today at 11:02"): the document is written once and read
/// for years. `datetime` carries the machine-readable value, so a UI
/// that grows a per-user format setting can restyle every stamp without
/// re-rendering anything.
pub fn timestamp_html(date_ms: Option<i64>) -> String {
    match date_ms.and_then(IsoOffsetTimestamp::from_unix_millis) {
        Some(t) => format!(
            "<time class=\"msg-ts\" datetime=\"{iso}\" title=\"{full}\">{short}</time>",
            iso = t.to_rfc3339_secs(),
            full = display_ts(date_ms),
            short = datalib_time::short_ts(&t),
        ),
        // Either no stamp at all or a number that is not an instant.
        // `display_ts` spells those two apart, and neither has an ISO
        // form to hang a `<time>` on.
        None => format!("<span class=\"msg-ts\">{}</span>", display_ts(date_ms)),
    }
}

/// The long form, shown on hover.
pub fn display_ts(date_ms: Option<i64>) -> String {
    datalib_time::display_ts_from_unix_millis(date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_shape() {
        let h = MessageHeader {
            author: "Picard",
            date_ms: Some(12442118400000),
            source_url: None,
            handle: None,
        };
        assert_eq!(
            h.render(),
            "## <span class=\"msg-author\">Picard</span> \
             <time class=\"msg-ts\" datetime=\"2364-04-11T00:00:00+00:00\" \
             title=\"2364-04-11 00:00:00 UTC\">Sat Apr 11th, 2364 at 00:00</time>",
        );
    }

    #[test]
    fn an_author_named_in_markup_cannot_break_out() {
        let h = MessageHeader {
            author: "<script>x</script> & co",
            date_ms: None,
            source_url: None,
            handle: None,
        };
        let s = h.render();
        assert!(s.contains("&lt;script&gt;x&lt;/script&gt; &amp; co"), "{s}");
        assert!(!s.contains("<script>"), "{s}");
        assert!(
            s.contains("<span class=\"msg-ts\">(no timestamp)</span>"),
            "{s}"
        );
    }

    /// markdown-it parses the text between the span's tags as markdown,
    /// so an HTML escape alone let a sender named `[x](url)` become a
    /// link (#992).
    #[test]
    fn an_author_named_in_markdown_is_not_a_link() {
        let h = MessageHeader {
            author: "[x](https://e.test) ![](https://t.test/i.png) `a|b`",
            date_ms: None,
            source_url: None,
            handle: None,
        };
        assert!(
            h.render().starts_with(
                "## <span class=\"msg-author\">\\[x\\](https://e.test) \
                 !\\[\\](https://t.test/i.png) \\`a\\|b\\`</span> "
            ),
            "{}",
            h.render()
        );
    }

    #[test]
    fn a_linkout_trails_the_stamp() {
        let h = MessageHeader {
            author: "Picard",
            date_ms: Some(12442118400000),
            source_url: Some("https://example.invalid/m/1?a=b&c=d"),
            handle: None,
        };
        let s = h.render();
        assert!(s.ends_with("noreferrer\">↗</a>"), "{s}");
        assert!(
            s.contains("href=\"https://example.invalid/m/1?a=b&amp;c=d\""),
            "{s}"
        );
    }

    /// The handle is what lets a link made after this document rendered
    /// still find the author, so it reaches the markdown as the href;
    /// the title is the hover every other viewer shows.
    #[test]
    fn an_author_with_a_handle_is_a_chip_link() {
        let riker = Handle::email("riker@enterprise.org").unwrap();
        let h = MessageHeader {
            author: "Will Riker",
            handle: Some(&riker),
            date_ms: None,
            source_url: None,
        };
        assert!(
            h.render().starts_with(
                "## [Will Riker](mailto:riker@enterprise.org \"Will Riker <riker@enterprise.org>\") "
            ),
            "{}",
            h.render()
        );
    }

    /// The link text is escaped as any header author is, and the title
    /// cannot close its own quotes.
    #[test]
    fn a_chip_link_escapes_its_text_and_its_title() {
        let q = Handle::email("q@continuum.org").unwrap();
        assert_eq!(
            chip_link("[Q] \"the\" <entity>", &q),
            "[\\[Q\\] \"the\" &lt;entity&gt;](mailto:q@continuum.org \
             \"[Q] \\\"the\\\" <entity> <q@continuum.org>\")"
        );
        assert_eq!(
            chip_link("", &q),
            "[q@continuum.org](mailto:q@continuum.org \"q@continuum.org\")"
        );
        let tel = Handle::tel("+15550123456").unwrap();
        assert_eq!(
            chip_link("Will", &tel),
            "[Will](tel:+15550123456 \"Will (+15550123456)\")"
        );
    }
}
