//! Where a document's embedded chunks fall. Whole sections are packed up to
//! a budget; only a section longer than the budget is cut inside itself,
//! at the last paragraph, line, sentence or word break before the limit,
//! and its pieces overlap. A chunk therefore names the sections it covers.
//!
//! A heading of level one or two starts a chunk, inside a section or
//! between them, so an edit under one heading moves only the chunks under
//! it. A chunk that holds less than a quarter of the budget takes the
//! heading in rather than standing alone, so a document's front matter
//! stays with its title.
//!
//! The budget is in bytes. qmd's is 900 tokens, which it reads as 3,600
//! characters; counting bytes keeps text with many multi-byte characters
//! (where a character is nearer a token) further inside the model's window.

pub const MAX_BYTES: usize = 3600;
pub const OVERLAP_BYTES: usize = 540;

/// A span of the document, its sections laid end to end, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub start: usize,
    pub end: usize,
    pub first_section: usize,
    pub last_section: usize,
}

/// blake3 of exactly what the model is shown for a chunk
/// ([`crate::prompt::document`]: the prompt's frame, the document's title,
/// the chunk's text), as hex. Its vector is kept under this and the
/// model's name, so a chunk whose prompt is unchanged keeps its vector
/// wherever it moves.
pub fn chunk_hash(title: &str, text: &str) -> String {
    let prompt = crate::prompt::document(text, Some(title));
    blake3::hash(prompt.as_bytes()).to_hex().to_string()
}

pub fn chunks(sections: &[&str]) -> Vec<Chunk> {
    chunks_within(sections, MAX_BYTES, OVERLAP_BYTES)
}

fn chunks_within(sections: &[&str], max: usize, overlap: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut open: Option<Chunk> = None;
    let mut at = 0;
    for (i, text) in sections.iter().enumerate() {
        let base = at;
        at += text.len();
        for (s, e, heading) in segments(text) {
            let piece = &text[s..e];
            let (start, end) = (base + s, base + e);
            if piece.trim().is_empty() {
                continue;
            }
            if heading && open.is_some_and(|c| c.end - c.start >= max / 4) {
                out.extend(open.take());
            }
            if piece.len() > max {
                out.extend(open.take());
                out.extend(cut(piece, max, overlap).into_iter().map(|(ps, pe)| Chunk {
                    start: start + ps,
                    end: start + pe,
                    first_section: i,
                    last_section: i,
                }));
                continue;
            }
            if let Some(c) = open.as_mut().filter(|c| end - c.start <= max) {
                c.end = end;
                c.last_section = i;
                continue;
            }
            out.extend(open.take());
            open = Some(Chunk {
                start,
                end,
                first_section: i,
                last_section: i,
            });
        }
    }
    out.extend(open);
    out
}

/// `text` split before each heading of level one or two outside a code
/// fence: `(start, end, starts with a heading)`.
fn segments(text: &str) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let mut from = 0;
    let mut from_heading = false;
    let mut pos = 0;
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && is_top_heading(line) {
            if pos > from {
                out.push((from, pos, from_heading));
            }
            from = pos;
            from_heading = true;
        }
        pos += line.len();
    }
    if text.len() > from {
        out.push((from, text.len(), from_heading));
    }
    out
}

fn is_top_heading(line: &str) -> bool {
    let line = line.trim_end_matches(['\n', '\r']);
    ["# ", "## "].iter().any(|h| line.starts_with(h)) || line == "#" || line == "##"
}

/// `text` in pieces of at most `max` bytes, each starting `overlap` bytes
/// before the last one ended, on character boundaries.
fn cut(text: &str, max: usize, overlap: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut from = 0;
    while text.len() - from > max {
        let hi = floor_boundary(text, from + max);
        let lo = floor_boundary(text, (hi - max / 5).max(from));
        let to = break_in(text, lo, hi);
        pieces.push((from, to));
        let next = ceil_boundary(text, to.saturating_sub(overlap));
        from = if next > from { next } else { to };
    }
    pieces.push((from, text.len()));
    pieces
}

/// The end of the last break in `text[lo..hi]`, the strongest kind found,
/// or `hi` when there is none.
fn break_in(text: &str, lo: usize, hi: usize) -> usize {
    let window = &text[lo..hi];
    ["\n\n", "\n", ". ", " "]
        .iter()
        .find_map(|b| window.rfind(b).map(|i| lo + i + b.len()))
        .unwrap_or(hi)
}

fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(sections: &[&str], max: usize, overlap: usize) -> Vec<(usize, usize, usize, usize)> {
        chunks_within(sections, max, overlap)
            .into_iter()
            .map(|c| (c.start, c.end, c.first_section, c.last_section))
            .collect()
    }

    #[test]
    fn short_sections_pack_into_one_chunk() {
        assert_eq!(
            spans(&["head\n", "one\n", "two\n"], 100, 10),
            [(0, 13, 0, 2)]
        );
    }

    #[test]
    fn a_full_chunk_closes_at_a_section_boundary() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        assert_eq!(
            spans(&[&a, &b, &c], 100, 10),
            [(0, 80, 0, 1), (80, 120, 2, 2)]
        );
    }

    #[test]
    fn a_blank_section_starts_no_chunk() {
        assert_eq!(spans(&["", "  \n", "words"], 100, 10), [(3, 8, 2, 2)]);
        assert!(spans(&[], 100, 10).is_empty());
    }

    /// A message longer than the budget is cut inside itself, at a
    /// paragraph break when one is near the limit, and the pieces overlap
    /// and cover all of it; its neighbours keep chunks of their own.
    #[test]
    fn a_long_section_is_cut_at_a_paragraph_and_overlaps() {
        let long = format!("{}\n\n{}", "x".repeat(90), "y".repeat(60));
        let got = spans(&["before", &long, "after"], 100, 20);
        assert_eq!(got[0], (0, 6, 0, 0));
        assert_eq!(got[1], (6, 6 + 92, 1, 1), "cut after the blank line");
        assert_eq!(got[2].0, 6 + 92 - 20, "the next piece overlaps");
        assert_eq!(got[2].1, 6 + long.len());
        assert_eq!(got[3], (6 + long.len(), 6 + long.len() + 5, 2, 2));
        assert!(got.iter().all(|(s, e, _, _)| e - s <= 100));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let long = "é".repeat(300);
        let pieces = cut(&long, 101, 15);
        assert!(pieces.len() > 1);
        for (s, e) in &pieces {
            assert!(long.is_char_boundary(*s) && long.is_char_boundary(*e));
            assert!(e - s <= 101);
        }
        assert_eq!(pieces.last().unwrap().1, long.len());
    }

    #[test]
    fn a_heading_inside_a_section_starts_a_chunk() {
        let first = format!("## One\n{}\n", "a".repeat(60));
        let second = format!("## Two\n{}\n", "b".repeat(60));
        let doc = format!("{first}{second}");
        assert_eq!(
            spans(&[&doc], 200, 10),
            [(0, first.len(), 0, 0), (first.len(), doc.len(), 0, 0)]
        );
    }

    #[test]
    fn a_heading_section_closes_the_open_chunk() {
        let a = "a".repeat(60);
        let b = "# Notes\nmore".to_string();
        assert_eq!(spans(&[&a, &b], 200, 10), [(0, 60, 0, 0), (60, 72, 1, 1)]);
    }

    /// Front matter ahead of the title is too small to stand alone, and a
    /// `###`, or a `#` inside a code fence, is no chunk boundary.
    #[test]
    fn small_chunks_deep_headings_and_fenced_lines_take_the_heading_in() {
        let doc = "---\ntitle: x\n---\n# Title\ntext\n### Deep\nmore\n```\n# shell comment\n```\n";
        assert_eq!(spans(&[doc], 200, 10), [(0, doc.len(), 0, 0)]);
    }

    /// An edit under one heading leaves the chunks under the others as
    /// they were, though it changes what comes after it.
    #[test]
    fn an_edit_under_one_heading_moves_only_its_chunks() {
        let body = |t: &str| format!("{}\n", t.repeat(60));
        let before = format!("# A\n{}# B\n{}# C\n{}", body("a"), body("b"), body("c"));
        let after = format!("# A\n{}# B\n{}# C\n{}", body("a"), body("bb"), body("c"));
        let texts = |d: &str| -> Vec<String> {
            spans(&[d], 200, 10)
                .into_iter()
                .map(|(s, e, _, _)| d[s..e].to_string())
                .collect()
        };
        let (old, new) = (texts(&before), texts(&after));
        assert_eq!(old.len(), 3);
        assert_eq!(new.len(), 3);
        assert_eq!(old[0], new[0]);
        assert_ne!(old[1], new[1]);
        assert_eq!(old[2], new[2]);
    }

    #[test]
    fn a_chunk_hash_is_the_hash_of_its_prompt() {
        let prompt = crate::prompt::document("x", Some("T"));
        assert_eq!(
            chunk_hash("T", "x"),
            blake3::hash(prompt.as_bytes()).to_hex().to_string()
        );
        assert_ne!(chunk_hash("T", "x"), chunk_hash("U", "x"));
    }
}
