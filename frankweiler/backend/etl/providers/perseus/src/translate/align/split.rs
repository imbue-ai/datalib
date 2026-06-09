//! Rule-based sentence splitter for Greek and English.
//!
//! Treats a terminator (`.` `;` `·` `:` for Greek; `.` `?` `!` for
//! English) followed by whitespace and an uppercase letter as a
//! sentence boundary. Keeps the terminator on the preceding sentence
//! so the concatenation of `(splits + interleaved whitespace)` is
//! lossless with respect to the input modulo the very whitespace runs
//! we collapsed.
//!
//! Returns `(sentence_text, byte_range_within_input)` so the renderer
//! can wrap each sentence in its own `<span>` without scanning the
//! input twice.
//!
//! NOT a general-purpose splitter: it's tuned for normalized Perseus
//! section text (single-line, no abbreviations, no acronyms). For the
//! ~3.6k Thucydides sections this gets the same splits as the
//! reference Python regex.

const GRC_TERMINATORS: &[char] = &['.', ';', '·', ':'];
const ENG_TERMINATORS: &[char] = &['.', '?', '!'];

/// One sentence span within a section's normalized text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    /// Sentence text, including its trailing terminator. Leading and
    /// trailing whitespace is trimmed.
    pub text: String,
    /// Byte range within the input string. `&input[start..end] == text`
    /// before trimming; the trimming only ever removes whitespace on
    /// the ends, so the range is still useful for `wrap_sentences` to
    /// position spans.
    pub start: usize,
    pub end: usize,
}

pub fn split_grc(text: &str) -> Vec<Sentence> {
    split(text, GRC_TERMINATORS)
}

pub fn split_eng(text: &str) -> Vec<Sentence> {
    split(text, ENG_TERMINATORS)
}

fn split(text: &str, terminators: &[char]) -> Vec<Sentence> {
    let trimmed_input = text.trim();
    if trimmed_input.is_empty() {
        return Vec::new();
    }

    // Walk the bytes; cut after a terminator + whitespace + uppercase
    // letter. Greek uppercase covers U+0391..=U+03A9 (and the Coptic
    // extensions which classical texts won't hit); we also accept the
    // polytonic upper-range U+1F00.. through U+1FFF where applicable.
    let bytes = text.as_bytes();
    let mut sentences: Vec<Sentence> = Vec::new();
    let mut sentence_start = match text.char_indices().find(|(_, c)| !c.is_whitespace()) {
        Some((i, _)) => i,
        None => return Vec::new(),
    };

    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !terminators.contains(&c) {
            continue;
        }
        // Lookahead: at least one whitespace char, then a char that
        // looks like a sentence-start (uppercase / capital).
        let mut k = chars.clone();
        let mut saw_ws = false;
        while let Some(&(_, nc)) = k.peek() {
            if nc.is_whitespace() {
                saw_ws = true;
                k.next();
            } else {
                break;
            }
        }
        if !saw_ws {
            continue;
        }
        let next_char = k.peek().map(|&(_, nc)| nc);
        let Some(nc) = next_char else {
            continue;
        };
        if !is_sentence_start(nc) {
            continue;
        }
        // Cut after the terminator (include it in the preceding
        // sentence). `i` is the byte offset of `c`; advance past it.
        let cut_end = i + c.len_utf8();
        let raw = &text[sentence_start..cut_end];
        sentences.push(make_sentence(raw, sentence_start));
        // Skip past the whitespace run; that's the start of the next
        // sentence.
        sentence_start = match k.peek() {
            Some(&(idx, _)) => idx,
            None => bytes.len(),
        };
    }

    // Tail.
    if sentence_start < text.len() {
        let raw = &text[sentence_start..];
        if !raw.trim().is_empty() {
            sentences.push(make_sentence(raw, sentence_start));
        }
    }

    sentences
}

fn is_sentence_start(c: char) -> bool {
    // Match the Python reference regex character class
    // `[A-ZΑ-Ωἀ-῿]` byte-for-byte. Includes:
    //   * ASCII A-Z (English sentence starts)
    //   * Greek basic capitals Α-Ω (U+0391..U+03A9)
    //   * The entire polytonic Greek range U+1F00..U+1FFD —
    //     intentionally permissive: Perseus capitals-with-diacritics
    //     (e.g. Ἀ U+1F08) live here, and the range also includes the
    //     lowercase polytonic letters. The Python build of the gold
    //     fixture matched on this widened class, so the Rust splitter
    //     must too to stay byte-stable with the reference; tightening
    //     it later would shift alignments and force a fixture rebuild.
    matches!(c, 'A'..='Z' | 'Α'..='Ω' | '\u{1F00}'..='\u{1FFD}')
}

fn make_sentence(raw: &str, raw_start: usize) -> Sentence {
    // Trim outer whitespace but keep the raw start/end so the
    // renderer can position the wrapping span correctly.
    let leading_ws: usize = raw
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace())
        .map(|(_, c)| c.len_utf8())
        .sum();
    let trailing_ws: usize = raw
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_whitespace())
        .map(|(_, c)| c.len_utf8())
        .sum();
    let trimmed = &raw[leading_ws..raw.len().saturating_sub(trailing_ws)];
    Sentence {
        text: trimmed.to_string(),
        start: raw_start + leading_ws,
        end: raw_start + raw.len() - trailing_ws,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_returns_no_sentences() {
        assert!(split_grc("").is_empty());
        assert!(split_eng("").is_empty());
        assert!(split_grc("   \n  ").is_empty());
    }

    #[test]
    fn single_sentence_no_split() {
        let s = split_eng("This is a single sentence.");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text, "This is a single sentence.");
    }

    #[test]
    fn english_two_sentences() {
        let s = split_eng("First one. Second one!");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "First one.");
        assert_eq!(s[1].text, "Second one!");
    }

    #[test]
    fn english_does_not_split_lowercase_after_period() {
        // Smith uses ". " inside lists with lowercase continuation
        // sometimes — those should NOT split.
        let s = split_eng("alpha. beta. Gamma.");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "alpha. beta.");
        assert_eq!(s[1].text, "Gamma.");
    }

    #[test]
    fn greek_recognises_middle_dot_terminator() {
        // Greek `·` is the semicolon; followed by capital Δ.
        let s = split_grc("καὶ ἔπραξε καλῶς· Δηλωθήσεται.");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "καὶ ἔπραξε καλῶς·");
        assert_eq!(s[1].text, "Δηλωθήσεται.");
    }

    #[test]
    fn greek_question_mark_is_semicolon_char() {
        // Greek `;` is the question mark.
        let s = split_grc("τίς οὖτος; Ἀθηναῖος.");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "τίς οὖτος;");
        assert_eq!(s[1].text, "Ἀθηναῖος.");
    }

    #[test]
    fn byte_ranges_round_trip_for_greek_input() {
        let input = "πρώτη φράσις. Δεύτερη φράσις.";
        let s = split_grc(input);
        for sent in &s {
            assert_eq!(&input[sent.start..sent.end], sent.text);
        }
    }

    #[test]
    fn three_sentences_with_internal_quote_terminator() {
        let s = split_eng("Alpha. Beta? Gamma!");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].text, "Alpha.");
        assert_eq!(s[1].text, "Beta?");
        assert_eq!(s[2].text, "Gamma!");
    }

    /// Smith opens Thucydides 1.1.3 with a `;` (the actual English
    /// punctuation — the Greek source uses `·`). Treat both as ends.
    #[test]
    fn english_semicolon_does_not_split() {
        // English `;` is NOT a sentence terminator here.
        let s = split_eng("the events of the period; and a still earlier date.");
        assert_eq!(s.len(), 1);
    }
}
