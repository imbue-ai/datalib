//! Character references read back to the text they stand for, over
//! quick-xml's table of HTML5's names. Lenient, as HTML is: a reference
//! this does not know, and a bare `&`, stay as written, and text is
//! decoded once, so `&amp;lt;` reads `&lt;`.

use std::borrow::Cow;

/// `s` with every reference it holds read: `&amp;`, `&nbsp;` (a no-break
/// space, U+00A0), `&#39;`, `&#x2014;`.
pub fn decode(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match reference_at(rest) {
            Some((text, len)) => {
                out.push_str(&text);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The longest name HTML5 gives a reference, `&CounterClockwiseContourIntegral;`.
const LONGEST: usize = 33;

/// The reference `s` opens with, read, and its length; `None` when the
/// `&` opens none this knows.
fn reference_at(s: &str) -> Option<(String, usize)> {
    let name_len = s[1..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
        .filter(|n| (1..=LONGEST).contains(n))?;
    let end = 1 + name_len;
    if !s[end..].starts_with(';') {
        return None;
    }
    let name = &s[1..end];
    let reference = match name.strip_prefix("#X") {
        Some(hex) => Cow::Owned(format!("&#x{hex};")),
        None => Cow::Borrowed(&s[..=end]),
    };
    let text = quick_xml::escape::unescape(&reference).ok()?;
    Some((text.into_owned(), end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_numeric_references_are_read_once() {
        assert_eq!(decode("a &amp; b"), "a & b");
        assert_eq!(
            decode("I&#39;ve &quot;seen&quot; &#x2014; &#X2014; &lt;it&gt;"),
            "I've \"seen\" \u{2014} \u{2014} <it>"
        );
        assert_eq!(decode("&amp;#39; &amp;lt;"), "&#39; &lt;", "decoded once");
        assert_eq!(decode("Captain&nbsp;Picard"), "Captain\u{a0}Picard");
        assert_eq!(
            decode("Engage&hellip; &mdash; &emsp;"),
            "Engage\u{2026} \u{2014} \u{2003}"
        );
    }

    #[test]
    fn what_is_not_a_reference_stays_as_written() {
        for s in [
            "Q & A",
            "&bogus; &",
            "AT&T;",
            "&#0; &#xFFFFFFFF;",
            "&amp",
            "& amp;",
            "&;",
        ] {
            assert_eq!(decode(s), s);
        }
        assert!(matches!(decode("no references"), Cow::Borrowed(_)));
    }
}
