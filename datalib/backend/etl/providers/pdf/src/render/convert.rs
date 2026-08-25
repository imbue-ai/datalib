//! PDF → Markdown conversion, plus the post-processing that the raw
//! converter output needs before it is worth indexing.
//!
//! # Why post-process at all
//!
//! pdf-inspector's markdown is good, but two of its artifacts are
//! actively harmful *to a search index* specifically, which is what we
//! feed. Both were found by running a mixed corpus through it (see
//! `PROTOTYPE.md`):
//!
//! 1. **Per-glyph CJK spacing.** A PDF that justifies CJK text
//!    positions each glyph separately, so extraction yields
//!    `世 界 人 权 宣 言` rather than `世界人权宣言`. Every substring
//!    search for the real word then misses. Measured at 307 occurrences
//!    in a single 7-page Chinese document.
//! 2. **Underline markup.** `detect_underline` emits raw `<u>` tags
//!    around every hyperlink in browser print-to-PDF output. They are
//!    not markdown, they clutter the text column of every grid row, and
//!    the link itself is already preserved as a markdown link. We turn
//!    the option off rather than strip after the fact.
//! 3. **Browser print chrome.** A page printed to PDF from a browser
//!    carries `8/24/26, 1:55 PM  Page Title` at the top of every page
//!    and `https://…  2/5` at the bottom. Indexed, that is one spurious
//!    hit per page for any query matching the title or the URL.
//!
//! pdf-inspector's own `strip_headers_footers` (on by default, left on)
//! removes most of these, but not all: 48 survived across the 4
//! print-to-PDF documents in the corpus. [`strip_repeated_chrome`]
//! catches the remainder that sit on their own line, by repetition
//! rather than by pattern.
//!
//! **What is still not handled, measured:** of those 48, only 8 were on
//! their own line; the other 40 had been *fused into a body line* by
//! the extractor (`… to honour 8/24/26, 1:55 PM Apollo. Over time …`),
//! where the same float-interleaving that scrambles Wikipedia infoboxes
//! puts them mid-paragraph. Removing those means editing inside a line
//! on a timestamp-shaped regex, which would eventually delete a real
//! date out of real prose. We leave them: a spurious per-page hit is a
//! smaller harm than silently corrupting document text. The real fix is
//! upstream reading-order work, not a bigger regex here.

use anyhow::{Context, Result};
use std::path::Path;

/// Bumped when conversion output changes in a way that should
/// invalidate previously-rendered documents. Feeds
/// `markdowns.renderer_version` via `RenderedMarkdown::render_version`.
///
/// Because it participates in the render cache key, this is also the
/// hook that makes an OCR engine swappable later: turning OCR on, or
/// changing engines, bumps this and every affected document re-renders
/// with no migration.
pub const RENDER_VERSION: u32 = 1;

/// One page of converted text.
pub struct Page {
    /// 1-indexed page number as reported by the converter.
    pub number: u32,
    pub text: String,
}

/// Convert one PDF and split the result into pages.
pub fn convert(path: &Path) -> Result<Vec<Page>> {
    let md = pdf_inspector::MarkdownOptions {
        // We split on these markers to build per-page sections, so they
        // are required, not cosmetic.
        include_page_numbers: true,
        // See §"Why post-process at all" item 2.
        detect_underline: false,
        // Left on because it does remove *some* running heads, but it
        // is not sufficient on its own — see the module docs and
        // `strip_repeated_chrome` below.
        strip_headers_footers: true,
        ..pdf_inspector::MarkdownOptions::default()
    };

    let opts = pdf_inspector::PdfOptions {
        markdown: md,
        ..pdf_inspector::PdfOptions::new()
    };
    let res = pdf_inspector::process_pdf_with_options(path, opts)
        .map_err(|e| anyhow::anyhow!("convert {}: {e}", path.display()))?;
    let raw = res
        .markdown
        .with_context(|| format!("no markdown produced for {}", path.display()))?;
    let mut pages = split_pages(&collapse_cjk_spacing(&raw));
    strip_repeated_chrome(&mut pages);
    pages.retain(|p| !p.text.trim().is_empty());
    Ok(pages)
}

/// Remove running heads and feet: the first and/or last line of a page
/// when that same line recurs on most other pages.
///
/// Keyed on repetition rather than on a pattern, because the shapes
/// vary by producer (browser print chrome, a report's running title, a
/// confidentiality footer) and a regex per shape would be an endless
/// list. Two constraints keep it from eating real content:
///
/// * **Position.** Only the first and last non-empty line of a page are
///   candidates. A sentence that legitimately repeats mid-body is
///   never touched.
/// * **Frequency.** The line must appear in that position on at least
///   half the pages, and on at least two. A single-page document is
///   left entirely alone — with one page there is no evidence any line
///   is chrome.
///
/// The two positions compare differently, and the asymmetry is
/// load-bearing:
///
/// * **Footers** are compared with digit runs normalized to `#`, so
///   `… 1/7` and `… 2/7` count as one footer. Pagination lives here.
/// * **Headers** must match *exactly*. Normalizing digits at the top of
///   the page would fuse `# Chapter 1` with `# Chapter 2` and delete
///   every chapter heading in a book — caught by
///   `keeps_headings_that_differ_per_page`. Browser print headers carry
///   a fixed timestamp and title, so they match exactly anyway.
pub fn strip_repeated_chrome(pages: &mut [Page]) {
    if pages.len() < 2 {
        return;
    }
    let threshold = std::cmp::max(2, pages.len() / 2);

    // Candidate → how many pages carry it in that position.
    let mut head_counts: std::collections::HashMap<String, usize> = Default::default();
    let mut foot_counts: std::collections::HashMap<String, usize> = Default::default();
    for p in pages.iter() {
        let (h, f) = edge_lines(&p.text);
        if let Some(h) = h {
            *head_counts.entry(h.trim().to_string()).or_default() += 1;
        }
        if let Some(f) = f {
            *foot_counts.entry(normalize_digits(f)).or_default() += 1;
        }
    }

    for p in pages.iter_mut() {
        let mut lines: Vec<&str> = p.text.lines().collect();
        // Trailing edge first, so removing it cannot shift the leading index.
        if let Some(idx) = lines.iter().rposition(|l| !l.trim().is_empty()) {
            if foot_counts
                .get(&normalize_digits(lines[idx]))
                .is_some_and(|&c| c >= threshold)
            {
                lines.remove(idx);
            }
        }
        if let Some(idx) = lines.iter().position(|l| !l.trim().is_empty()) {
            if head_counts
                .get(lines[idx].trim())
                .is_some_and(|&c| c >= threshold)
            {
                lines.remove(idx);
            }
        }
        p.text = lines.join("\n").trim().to_string();
    }
}

/// First and last non-empty lines of a page, if any.
fn edge_lines(text: &str) -> (Option<&str>, Option<&str>) {
    let mut non_empty = text.lines().filter(|l| !l.trim().is_empty());
    let first = non_empty.next();
    let last = non_empty.next_back().or(first);
    (first, last)
}

/// Collapse digit runs so paginated variants of one footer compare
/// equal (`… 1/7` and `… 2/7` both become `… #/#`).
fn normalize_digits(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.trim().chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// Split converter output on its `<!-- Page N -->` markers.
///
/// Content before the first marker (some documents emit none at all)
/// becomes page 1, so a document always has at least one page as long
/// as it has any text.
pub fn split_pages(md: &str) -> Vec<Page> {
    let mut pages: Vec<Page> = Vec::new();
    let mut current = String::new();
    let mut number: u32 = 1;

    for line in md.lines() {
        if let Some(n) = parse_page_marker(line) {
            if !current.trim().is_empty() {
                pages.push(Page {
                    number,
                    text: current.trim().to_string(),
                });
            }
            current.clear();
            number = n;
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        pages.push(Page {
            number,
            text: current.trim().to_string(),
        });
    }
    pages
}

fn parse_page_marker(line: &str) -> Option<u32> {
    let t = line.trim();
    let inner = t.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
    let n = inner.strip_prefix("Page ")?;
    n.trim().parse().ok()
}

/// Collapse single spaces between CJK ideographs.
///
/// Only *single* spaces between two CJK characters are removed. A run
/// of two or more is a deliberate gap (table cell padding, a column
/// boundary the extractor preserved) and is left alone. Latin text
/// interleaved with CJK is unaffected, because at least one side of the
/// space is then not an ideograph.
pub fn collapse_cjk_spacing(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' && i > 0 && i + 1 < chars.len() {
            let prev = chars[i - 1];
            let next = chars[i + 1];
            if is_cjk(prev) && is_cjk(next) {
                i += 1; // drop this space
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Ideographic scripts whose text has no inter-word spaces, so a space
/// between two of them is an extraction artifact rather than a word
/// boundary. Deliberately excludes Hangul: Korean *does* space between
/// words, so collapsing there would corrupt real text.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x309F   // Hiragana
        | 0x30A0..=0x30FF // Katakana
        | 0x3400..=0x4DBF // CJK Ext A
        | 0x4E00..=0x9FFF // CJK Unified
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x20000..=0x2A6DF // CJK Ext B
    ) || matches!(
        c,
        '。' | '，' | '、' | '；' | '：' | '？' | '！' | '（' | '）' | '《' | '》'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_single_spaces_between_han() {
        assert_eq!(collapse_cjk_spacing("世 界 人 权"), "世界人权");
    }

    #[test]
    fn preserves_runs_of_two_or_more_spaces() {
        // A wide gap is a real layout boundary, not per-glyph justification.
        assert_eq!(collapse_cjk_spacing("世界  人权"), "世界  人权");
    }

    #[test]
    fn leaves_latin_text_alone() {
        assert_eq!(collapse_cjk_spacing("hello world"), "hello world");
    }

    #[test]
    fn leaves_mixed_boundaries_alone() {
        // Space between Han and Latin is a genuine separator.
        assert_eq!(collapse_cjk_spacing("世界 hello"), "世界 hello");
        assert_eq!(collapse_cjk_spacing("hello 世界"), "hello 世界");
    }

    #[test]
    fn does_not_collapse_hangul_which_spaces_between_words() {
        // Korean would be corrupted by collapsing; this is why Hangul
        // is excluded from `is_cjk`.
        let ko = "모든 인류 구성원의";
        assert_eq!(collapse_cjk_spacing(ko), ko);
    }

    #[test]
    fn collapses_japanese_kana() {
        assert_eq!(collapse_cjk_spacing("人 権 の"), "人権の");
    }

    #[test]
    fn splits_on_page_markers() {
        let md = "<!-- Page 1 -->\nalpha\n<!-- Page 2 -->\nbeta\n";
        let pages = split_pages(md);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].number, 1);
        assert_eq!(pages[0].text, "alpha");
        assert_eq!(pages[1].number, 2);
        assert_eq!(pages[1].text, "beta");
    }

    #[test]
    fn content_before_any_marker_becomes_page_one() {
        let pages = split_pages("no markers here\n");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].number, 1);
        assert_eq!(pages[0].text, "no markers here");
    }

    #[test]
    fn blank_pages_are_dropped_not_emitted_empty() {
        // A scanned insert between two text pages yields nothing; an
        // empty section would be a dead anchor in the UI.
        let md = "<!-- Page 1 -->\nalpha\n<!-- Page 2 -->\n\n<!-- Page 3 -->\ngamma\n";
        let pages = split_pages(md);
        assert_eq!(
            pages.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn page_numbers_follow_the_markers_not_the_sequence() {
        // `--select-pages`-style output can start at an arbitrary page.
        let pages = split_pages("<!-- Page 7 -->\nseven\n");
        assert_eq!(pages[0].number, 7);
    }

    fn pages(texts: &[&str]) -> Vec<Page> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| Page {
                number: i as u32 + 1,
                text: (*t).to_string(),
            })
            .collect()
    }

    #[test]
    fn strips_repeated_browser_print_header_and_footer() {
        // The shape measured on real print-to-PDF output: identical
        // timestamped head, paginated foot.
        let mut p = pages(&[
            "8/24/26, 1:55 PM Rust Blog\nreal content one\nhttps://example.com/x 1/3",
            "8/24/26, 1:55 PM Rust Blog\nreal content two\nhttps://example.com/x 2/3",
            "8/24/26, 1:55 PM Rust Blog\nreal content three\nhttps://example.com/x 3/3",
        ]);
        strip_repeated_chrome(&mut p);
        assert_eq!(p[0].text, "real content one");
        assert_eq!(p[1].text, "real content two");
        assert_eq!(p[2].text, "real content three");
    }

    #[test]
    fn leaves_a_single_page_document_untouched() {
        // One page is no evidence that anything is chrome.
        let mut p = pages(&["8/24/26, 1:55 PM Title\nbody\nfooter 1/1"]);
        strip_repeated_chrome(&mut p);
        assert!(p[0].text.contains("8/24/26"));
        assert!(p[0].text.contains("footer"));
    }

    #[test]
    fn does_not_strip_lines_that_merely_repeat_mid_body() {
        // Position matters: only page-edge lines are candidates.
        let mut p = pages(&[
            "head a\nSTATUS: OK\ntail a",
            "head b\nSTATUS: OK\ntail b",
            "head c\nSTATUS: OK\ntail c",
        ]);
        strip_repeated_chrome(&mut p);
        assert!(p.iter().all(|x| x.text.contains("STATUS: OK")));
        // ...and distinct edges survive too.
        assert!(p[0].text.contains("head a"));
        assert!(p[0].text.contains("tail a"));
    }

    #[test]
    fn keeps_headings_that_differ_per_page() {
        // Regression guard for the header/footer asymmetry: digit
        // normalization at the top of the page would fuse these two
        // and delete every chapter heading in a book.
        let mut p = pages(&["# Chapter 1\nalpha", "# Chapter 2\nbeta"]);
        strip_repeated_chrome(&mut p);
        assert!(p[0].text.contains("Chapter 1"));
        assert!(p[1].text.contains("Chapter 2"));
    }

    #[test]
    fn an_identical_running_head_is_still_stripped() {
        // The exact-match header rule must still catch the real case.
        let mut p = pages(&["ACME Confidential\nalpha", "ACME Confidential\nbeta"]);
        strip_repeated_chrome(&mut p);
        assert_eq!(p[0].text, "alpha");
        assert_eq!(p[1].text, "beta");
    }

    #[test]
    fn digit_normalization_matches_paginated_variants() {
        assert_eq!(normalize_digits("page 1/7"), normalize_digits("page 12/7"));
        assert_ne!(normalize_digits("page a"), normalize_digits("page b"));
    }

    #[test]
    fn a_page_reduced_to_nothing_by_stripping_is_dropped_by_convert() {
        // `convert` retains only non-empty pages; verify the stripper
        // can in fact empty one, which is the case that matters.
        let mut p = pages(&["running head", "running head"]);
        strip_repeated_chrome(&mut p);
        assert!(p.iter().all(|x| x.text.is_empty()));
    }

    #[test]
    fn non_page_comments_are_not_treated_as_markers() {
        let pages = split_pages("<!-- not a page -->\nbody\n");
        assert_eq!(pages.len(), 1);
        assert!(pages[0].text.contains("not a page"));
    }
}
