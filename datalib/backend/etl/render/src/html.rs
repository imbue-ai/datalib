//! Escaping for the small HTML fragments a renderer splices into its
//! markdown — the per-message header, a section wrapper, a link.
//!
//! Here rather than in `datalib_etl`: that crate sits upstream of ~130
//! test targets, and only the render side writes HTML.

/// Escape text that lands between tags. `&` first, or the escapes
/// this function just wrote get escaped again.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape a value going inside a double-quoted attribute.
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_markup_without_double_escaping() {
        assert_eq!(escape_text("Riker & Troi"), "Riker &amp; Troi");
        assert_eq!(escape_text("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
        // The ampersand of an escape we just emitted is not re-escaped,
        // which is what escaping `&` first buys.
        assert_eq!(escape_text("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn attr_escapes_the_quote_that_would_break_out() {
        assert_eq!(
            escape_attr("https://e.com/?a=b&c=\"d\""),
            "https://e.com/?a=b&amp;c=&quot;d&quot;"
        );
    }
}
