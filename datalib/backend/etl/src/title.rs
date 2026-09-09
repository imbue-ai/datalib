//! Cross-provider title block used at the top of every rendered `.md`.

use std::fmt::Write;

/// One title block. Pushed onto the rendered markdown body by every
/// provider in lieu of an open-coded `# {title}` line.
#[derive(Debug, Clone)]
pub struct Title<'a> {
    /// Display text. Plain string; HTML-escaped at render time so
    /// titles with `<` / `>` / `&` survive markdown-it's html-passthrough
    /// mode.
    pub text: &'a str,
    /// Stable id for the rendered markdown. The Vue side wires up a
    /// "Copy page ID" button against this; omitted when `None`. For
    /// every provider this is the same UUID that addresses
    /// `/api/chat/{markdown_uuid}` — pass `markdown_uuid` directly.
    pub markdown_uuid: Option<&'a str>,
    /// External link to the source artifact (`claude.ai/chat/…`,
    /// `chatgpt.com/c/…`, `github.com/owner/repo/pull/N`, …). The
    /// rendered `<a>` carries `target="_blank"` + `rel="noopener
    /// noreferrer"` so it always opens in a new tab without giving
    /// the source page a handle on the opener. Omitted when `None`.
    pub source_url: Option<&'a str>,
}

/// Longest heading we render in full.
///
/// A conversation upstream never named gets titled after its first
/// message — ChatGPT does this — so a page title can run to several
/// hundred characters and push everything else off the screen. The
/// full string is not lost: it goes in the `title` attribute, the
/// frontmatter, and `grid_rows.conversation_name`, which is what
/// search reads.
const MAX_TITLE_CHARS: usize = 90;

/// `(shown, full)` — `full` is `None` when nothing was cut.
fn clamp(text: &str) -> (String, Option<&str>) {
    if text.chars().count() <= MAX_TITLE_CHARS {
        return (text.to_string(), None);
    }
    // Cut on a word boundary when there is one near the limit, so the
    // heading does not end mid-word; fall back to the hard limit for a
    // string with no spaces at all (a URL, a hash).
    let hard: String = text.chars().take(MAX_TITLE_CHARS).collect();
    let cut = match hard.rfind(' ') {
        Some(i) if i >= MAX_TITLE_CHARS * 2 / 3 => &hard[..i],
        _ => hard.as_str(),
    };
    (format!("{}…", cut.trim_end()), Some(text))
}

impl<'a> Title<'a> {
    /// Render the title block as an HTML-in-markdown chunk. The
    /// returned string ends with `\n\n` so callers can splice it
    /// straight into the body without worrying about blank-line
    /// terminators.
    pub fn render(&self) -> String {
        let (shown, full) = clamp(self.text);
        let mut out = String::new();
        out.push_str("<h1 class=\"page-title\"");
        if let Some(uuid) = self.markdown_uuid {
            write!(out, " data-page-title-uuid=\"{}\"", escape_attr(uuid))
                .expect("write to String");
        }
        if let Some(full) = full {
            write!(out, " title=\"{}\"", escape_attr(full)).expect("write to String");
        }
        out.push('>');
        out.push_str(&escape_html(&shown));
        if let Some(url) = self.source_url {
            write!(
                out,
                " <a class=\"source-link\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a>",
                escape_attr(url),
            )
            .expect("write to String");
        }
        out.push_str("</h1>\n\n");
        out
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_only() {
        let t = Title {
            text: "Hello",
            markdown_uuid: None,
            source_url: None,
        };
        assert_eq!(t.render(), "<h1 class=\"page-title\">Hello</h1>\n\n");
    }

    #[test]
    fn title_with_uuid() {
        let t = Title {
            text: "Hello",
            markdown_uuid: Some("abc-123"),
            source_url: None,
        };
        assert_eq!(
            t.render(),
            "<h1 class=\"page-title\" data-page-title-uuid=\"abc-123\">Hello</h1>\n\n",
        );
    }

    #[test]
    fn title_with_url() {
        let t = Title {
            text: "Hello",
            markdown_uuid: None,
            source_url: Some("https://example.com/chat/x"),
        };
        assert_eq!(
            t.render(),
            "<h1 class=\"page-title\">Hello <a class=\"source-link\" href=\"https://example.com/chat/x\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a></h1>\n\n",
        );
    }

    #[test]
    fn title_with_uuid_and_url() {
        let t = Title {
            text: "Hello",
            markdown_uuid: Some("abc-123"),
            source_url: Some("https://example.com/chat/x"),
        };
        assert_eq!(
            t.render(),
            "<h1 class=\"page-title\" data-page-title-uuid=\"abc-123\">Hello <a class=\"source-link\" href=\"https://example.com/chat/x\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a></h1>\n\n",
        );
    }

    /// A conversation ChatGPT titled after its first message runs to
    /// hundreds of characters; rendered in full it is the whole top of
    /// the page. The full string stays reachable on hover.
    #[test]
    fn a_very_long_title_is_clamped_with_the_full_text_on_hover() {
        let long = "I have been reviewing the Daystrom Institute archives on subspace \
                    harmonic stabilization, and I'm increasingly convinced that our \
                    current dilithium recrystallization approach is a dead end";
        let s = Title {
            text: long,
            markdown_uuid: None,
            source_url: None,
        }
        .render();

        assert!(s.contains('…'), "{s}");
        assert!(
            s.contains("title=\"I have been reviewing the Daystrom Institute archives"),
            "the full text is on the element: {s}"
        );
        // Clamped on a word boundary, not mid-word.
        let shown = s
            .split_once('>')
            .and_then(|(_, rest)| rest.split_once("</h1>"))
            .map(|(t, _)| t.to_string())
            .expect("heading text");
        assert!(shown.chars().count() <= 91, "{shown:?}");
        assert!(shown.ends_with('…') && !shown.ends_with(" …"), "{shown:?}");
    }

    /// A title that fits is rendered verbatim, with no `title`
    /// attribute promising a fuller version that does not exist.
    #[test]
    fn a_short_title_is_left_alone() {
        let s = Title {
            text: "Bridge Crew",
            markdown_uuid: None,
            source_url: None,
        }
        .render();
        assert_eq!(s, "<h1 class=\"page-title\">Bridge Crew</h1>\n\n");
    }

    #[test]
    fn escapes_html_in_title() {
        let t = Title {
            text: "<script>alert('x')</script> & more",
            markdown_uuid: None,
            source_url: None,
        };
        assert_eq!(
            t.render(),
            "<h1 class=\"page-title\">&lt;script&gt;alert('x')&lt;/script&gt; &amp; more</h1>\n\n",
        );
    }

    #[test]
    fn escapes_quote_and_amp_in_attrs() {
        // Realistic-ish: a uuid never has these, but a misconfigured
        // source_url might carry `&query` or a stray quote. We escape
        // them so the attribute can't break out of its enclosing
        // double-quoted context.
        let t = Title {
            text: "x",
            markdown_uuid: None,
            source_url: Some("https://e.com/?a=b&c=\"d\""),
        };
        let s = t.render();
        assert!(
            s.contains("href=\"https://e.com/?a=b&amp;c=&quot;d&quot;\""),
            "expected escaped attrs in: {s}",
        );
    }

    #[test]
    fn newline_terminator() {
        // Callers splice straight into the body — the trailing `\n\n`
        // is a guaranteed paragraph terminator for markdown-it.
        let t = Title {
            text: "x",
            markdown_uuid: None,
            source_url: None,
        };
        assert!(t.render().ends_with("\n\n"));
    }
}
