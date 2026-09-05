//! PDF → Markdown conversion, plus the post-processing that the raw
//! converter output needs before it is worth indexing.

use anyhow::{Context, Result};
use std::path::Path;

/// Bumped when render output changes in a way that should invalidate
/// previously-rendered documents — the markdown itself, or the shape of
/// the `grid_rows` projected from it.
pub const RENDER_VERSION: u32 = 3;

/// One page of converted text.
pub struct Page {
    /// 1-indexed page number as reported by the converter.
    pub number: u32,
    pub text: String,
    /// True when this page produced no text and `text` is the
    /// placeholder note rather than content from the document. Such a
    /// page goes into the markdown so the gap is visible to a reader,
    /// but gets no `grid_rows` entry — see [`note_for_page`].
    pub non_textual: bool,
}

impl Page {
    fn textual(number: u32, text: String) -> Self {
        Self {
            number,
            text,
            non_textual: false,
        }
    }
}

/// What we write in place of a page we could not read.
pub fn note_for_page(number: u32) -> String {
    format!(
        "*Page {number} — no extractable text (image-only or scanned; OCR is not available yet).*"
    )
}

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
    let md_text = collapse_cjk_spacing(&raw);
    let mut pages = split_pages(&md_text);
    strip_repeated_chrome(&mut pages);
    pages.retain(|p| !p.text.trim().is_empty());
    note_unreadable_pages(&mut pages, res.page_count, saw_page_markers(&md_text));
    Ok(pages)
}

fn saw_page_markers(md: &str) -> bool {
    md.lines().any(|l| parse_page_marker(l).is_some())
}

fn note_unreadable_pages(pages: &mut Vec<Page>, page_count: u32, saw_markers: bool) {
    if !saw_markers || pages.is_empty() || page_count == 0 {
        return;
    }
    if pages.iter().any(|p| p.number > page_count) {
        return;
    }
    let have: std::collections::HashSet<u32> = pages.iter().map(|p| p.number).collect();
    for n in 1..=page_count {
        if !have.contains(&n) {
            pages.push(Page {
                number: n,
                text: note_for_page(n),
                non_textual: true,
            });
        }
    }
    pages.sort_by_key(|p| p.number);
}

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

fn edge_lines(text: &str) -> (Option<&str>, Option<&str>) {
    let mut non_empty = text.lines().filter(|l| !l.trim().is_empty());
    let first = non_empty.next();
    let last = non_empty.next_back().or(first);
    (first, last)
}

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

pub fn split_pages(md: &str) -> Vec<Page> {
    let mut pages: Vec<Page> = Vec::new();
    let mut current = String::new();
    let mut number: u32 = 1;

    for line in md.lines() {
        if let Some(n) = parse_page_marker(line) {
            if !current.trim().is_empty() {
                pages.push(Page::textual(number, current.trim().to_string()));
            }
            current.clear();
            number = n;
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        pages.push(Page::textual(number, current.trim().to_string()));
    }
    pages
}

fn parse_page_marker(line: &str) -> Option<u32> {
    let t = line.trim();
    let inner = t.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
    let n = inner.strip_prefix("Page ")?;
    n.trim().parse().ok()
}

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
            .map(|(i, t)| Page::textual(i as u32 + 1, (*t).to_string()))
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
    fn an_unreadable_page_gets_a_note_rather_than_disappearing() {
        // The Mixed shape: text on 1 and 3, an image-only insert on 2.
        let mut p = pages(&["alpha", "gamma"]);
        p[1].number = 3;
        note_unreadable_pages(&mut p, 3, true);
        assert_eq!(
            p.iter().map(|x| x.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(p[1].non_textual);
        assert!(p[1].text.contains("Page 2"), "{}", p[1].text);
        // The pages that did convert are untouched and still count as
        // real text, which is what gives them grid rows.
        assert!(!p[0].non_textual && !p[2].non_textual);
        assert_eq!(p[0].text, "alpha");
        assert_eq!(p[2].text, "gamma");
    }

    #[test]
    fn a_trailing_run_of_unreadable_pages_is_noted_too() {
        // Nothing tells us page 4 exists except the census, which is
        // why `note_unreadable_pages` takes it rather than inferring
        // the count from the highest marker.
        let mut p = pages(&["alpha"]);
        note_unreadable_pages(&mut p, 4, true);
        assert_eq!(p.len(), 4);
        assert!(p[1..].iter().all(|x| x.non_textual));
    }

    #[test]
    fn a_fully_readable_document_gains_no_notes() {
        let mut p = pages(&["alpha", "beta"]);
        note_unreadable_pages(&mut p, 2, true);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|x| !x.non_textual));
    }

    #[test]
    fn nothing_is_invented_when_the_numbering_cannot_be_trusted() {
        // No markers at all: `split_pages` calls the whole document page
        // 1, so a census of 5 would otherwise produce four notes
        // claiming pages we never looked at are blank.
        let md = "no markers here\n";
        assert!(!saw_page_markers(md));
        let mut p = split_pages(md);
        note_unreadable_pages(&mut p, 5, saw_page_markers(md));
        assert_eq!(
            p.len(),
            1,
            "a bare page-1 fallback must not be extrapolated"
        );

        // A marker past the end of the census — the two disagree, so we
        // trust neither.
        let mut q = split_pages("<!-- Page 7 -->\nseven\n");
        note_unreadable_pages(&mut q, 3, true);
        assert_eq!(q.len(), 1);

        // And an empty conversion stays empty rather than becoming a
        // document made entirely of notes.
        let mut empty: Vec<Page> = Vec::new();
        note_unreadable_pages(&mut empty, 9, true);
        assert!(empty.is_empty());
    }

    #[test]
    fn non_page_comments_are_not_treated_as_markers() {
        let pages = split_pages("<!-- not a page -->\nbody\n");
        assert_eq!(pages.len(), 1);
        assert!(pages[0].text.contains("not a page"));
    }
}
