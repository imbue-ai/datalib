//! Cross-provider title block used at the top of every rendered `.md`.
//!
//! Every provider's renderer used to open its file with a hand-rolled
//! `# {title}` H1 (Anthropic: `# {conv.name}`, ChatGPT:
//! `# {conv.title}`, Slack: `# #{channel}: {thread_title}`, GitHub:
//! `# {pr.title} (#{n})`, GitLab: `# {mr.title} (!{n})`, Notion:
//! `# {icon}{title}`, Beeper: `# {room.title} · {period}`). Each one
//! was duplicated by the Vue preview pane's `<h2>` header, so the
//! user saw the title twice. This module collapses that into a
//! single [`Title`] block — same HTML across providers — and the Vue
//! side decorates it (copy-id button) and removes its own redundant
//! header.
//!
//! Output is a self-contained HTML block that markdown-it (configured
//! with `html: true`) passes through verbatim:
//!
//! ```html
//! <h1 class="page-title" data-page-title-uuid="…">
//!   {title}
//!   <a class="source-link" href="…" target="_blank" rel="noopener noreferrer">↗</a>
//! </h1>
//! ```
//!
//! The `data-page-title-uuid` attribute is the hook the Vue side uses
//! to attach a copy-id button (mirroring the existing
//! `data-section-uuid` pattern for per-message buttons). The
//! `source-link` arrow is plain HTML — `target="_blank"` + `rel`
//! attributes mean it always opens in a new tab and never replaces
//! the preview pane.
//!
//! Either or both of `markdown_uuid` and `source_url` may be `None`
//! — the rendered block degrades to just the title.

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

impl<'a> Title<'a> {
    /// Render the title block as an HTML-in-markdown chunk. The
    /// returned string ends with `\n\n` so callers can splice it
    /// straight into the body without worrying about blank-line
    /// terminators.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("<h1 class=\"page-title\"");
        if let Some(uuid) = self.markdown_uuid {
            write!(out, " data-page-title-uuid=\"{}\"", escape_attr(uuid))
                .expect("write to String");
        }
        out.push('>');
        out.push_str(&escape_html(self.text));
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
