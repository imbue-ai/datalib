//! A marketing mail's preheader padding taken out of its body: long runs
//! of characters that show nothing (`U+034F`, zero-width joiners and
//! spaces, direction marks), which senders add so a mail client's preview
//! line shows no more than their chosen text. A lone one of them means
//! something (a zero-width joiner holds an emoji sequence together, a
//! direction mark orders Hebrew or Arabic), so only a run of several goes.

/// Characters that take no space on the page.
const INVISIBLE: &[char] = &[
    '\u{00AD}', '\u{034F}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{2060}',
    '\u{FEFF}',
];

/// What may sit between them in a run.
const BETWEEN: &[char] = &[' ', '\t', '\u{00A0}', '\u{2007}'];

/// The fewest invisible characters a run of padding holds.
const RUN: usize = 3;

/// `md` without its padding runs. A run between words leaves one space; a
/// line that held nothing else goes. Fenced code is left as written.
pub(crate) fn without_padding(md: &str) -> String {
    if !md.contains(INVISIBLE) {
        return md.to_string();
    }
    let mut out = String::with_capacity(md.len());
    let mut fenced = false;
    for line in md.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        let kept = line_without_padding(line);
        if kept.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        out.push_str(&kept);
    }
    out
}

fn line_without_padding(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let in_run = |c: char| INVISIBLE.contains(&c) || BETWEEN.contains(&c);
        if !in_run(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let end = (i..chars.len())
            .find(|&j| !in_run(chars[j]))
            .unwrap_or(chars.len());
        let run = &chars[i..end];
        if run.iter().filter(|c| INVISIBLE.contains(c)).count() >= RUN {
            let between_words = i > 0 && end < chars.len() && chars[end] != '\n';
            if between_words {
                out.push(' ');
            }
        } else {
            out.extend(run);
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A promotion's preheader padded its preview with `͏ ` runs, which
    /// the page and search both kept as a line of nothing.
    #[test]
    fn a_run_of_padding_goes_and_a_line_of_it_goes_whole() {
        let pad = "\u{034F}\u{00A0}\u{200C}\u{034F} \u{200C}\u{034F}\u{00A0}".repeat(4);
        assert_eq!(
            without_padding(&format!(
                "Shore leave on Risa\n\n{pad}\n\nBook by Friday.\n"
            )),
            "Shore leave on Risa\n\n\nBook by Friday.\n"
        );
        assert_eq!(
            without_padding(&format!("Engage{pad}now")),
            "Engage now",
            "a run between words leaves a space"
        );
    }

    #[test]
    fn a_lone_invisible_character_is_kept() {
        for text in [
            "The crew: \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} aboard",
            "Word\u{200E} and \u{05E9}\u{05DC}\u{05D5}\u{05DD}\u{200F}",
            "co\u{00AD}operation",
        ] {
            assert_eq!(without_padding(text), text);
        }
    }

    #[test]
    fn fenced_code_is_left_as_written() {
        let pad = "\u{034F}\u{200C}\u{034F}\u{200C}";
        let md = format!("```\n{pad}\n```\n");
        assert_eq!(without_padding(&md), md);
    }
}
