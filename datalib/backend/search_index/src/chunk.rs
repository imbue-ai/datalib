//! Where a document's embedded chunks fall. Whole sections are packed up to
//! a budget; only a section longer than the budget is cut inside itself,
//! at the last paragraph, line, sentence or word break before the limit,
//! and its pieces overlap. A chunk therefore names the sections it covers.
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

pub fn chunks(sections: &[&str]) -> Vec<Chunk> {
    chunks_within(sections, MAX_BYTES, OVERLAP_BYTES)
}

fn chunks_within(sections: &[&str], max: usize, overlap: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut open: Option<Chunk> = None;
    let mut at = 0;
    for (i, text) in sections.iter().enumerate() {
        let (start, end) = (at, at + text.len());
        at = end;
        if text.trim().is_empty() {
            continue;
        }
        if text.len() > max {
            out.extend(open.take());
            out.extend(cut(text, max, overlap).into_iter().map(|(s, e)| Chunk {
                start: start + s,
                end: start + e,
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
    out.extend(open);
    out
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
}
