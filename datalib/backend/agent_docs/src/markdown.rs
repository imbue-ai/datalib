//! Just enough of Markdown to find a doc's code blocks, headings and
//! links: fences opened by three backticks, `#` headings outside them,
//! and inline `[text](target)` links outside code.

use std::ops::Range;

pub struct Block<'a> {
    /// What follows the opening backticks: the language, and for TOML
    /// perhaps `fragment` (`toml fragment`).
    pub info: &'a str,
    /// The opening fence's line, counting from 1.
    pub line: usize,
    /// The lines between the fences, with the fence's indent taken off.
    pub body: String,
    range: Range<usize>,
}

pub fn fenced_blocks(markdown: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, usize, &str, usize, String)> = None;
    let mut offset = 0;
    for (n, line) in markdown.split_inclusive('\n').enumerate() {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        let indent = content.len() - trimmed.len();
        match open.take() {
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    open = Some((offset, n + 1, info.trim(), indent, String::new()));
                }
            }
            Some((start, first, info, fence_indent, mut body)) => {
                if trimmed.starts_with("```") && trimmed.trim_start_matches('`').trim().is_empty() {
                    blocks.push(Block {
                        info,
                        line: first,
                        body,
                        range: start..offset + line.len(),
                    });
                } else {
                    let cut = indent.min(fence_indent);
                    body.push_str(&content[cut..]);
                    body.push('\n');
                    open = Some((start, first, info, fence_indent, body));
                }
            }
        }
        offset += line.len();
    }
    blocks
}

/// Every heading's text, in order, outside code blocks.
pub fn headings(markdown: &str) -> Vec<&str> {
    let fences: Vec<Range<usize>> = fenced_blocks(markdown)
        .into_iter()
        .map(|b| b.range)
        .collect();
    let mut out = Vec::new();
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        let inside = fences.iter().any(|r| r.contains(&offset));
        offset += line.len();
        if inside {
            continue;
        }
        let hashes = line.len() - line.trim_start_matches('#').len();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            out.push(line[hashes..].trim());
        }
    }
    out
}

/// GitHub's anchor for a heading: lower case, spaces to `-`, and
/// punctuation other than `-` and `_` dropped.
pub fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

pub struct Link<'a> {
    pub text: &'a str,
    pub target: &'a str,
    /// The line the link's `](` is on, counting from 1.
    pub line: usize,
    /// The whole `[text](target)`.
    pub range: Range<usize>,
}

/// Every inline link outside code. An image (`![…](…)`) is not a link to
/// follow, and is left out.
pub fn links(markdown: &str) -> Vec<Link<'_>> {
    let fences: Vec<Range<usize>> = fenced_blocks(markdown)
        .into_iter()
        .map(|b| b.range)
        .collect();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = markdown[from..].find("](") {
        let mid = from + found;
        from = mid + 2;
        if fences.iter().any(|r| r.contains(&mid)) || in_code_span(markdown, mid) {
            continue;
        }
        let Some(close) = markdown[mid + 2..].find(')').map(|i| mid + 2 + i) else {
            break;
        };
        let Some(open) = opening_bracket(&markdown[..mid]) else {
            continue;
        };
        if markdown[..open].ends_with('!') {
            continue;
        }
        out.push(Link {
            text: &markdown[open + 1..mid],
            target: &markdown[mid + 2..close],
            line: markdown[..mid].matches('\n').count() + 1,
            range: open..close + 1,
        });
        from = close + 1;
    }
    out
}

fn in_code_span(markdown: &str, at: usize) -> bool {
    let line_start = markdown[..at].rfind('\n').map_or(0, |i| i + 1);
    markdown[line_start..at].matches('`').count() % 2 == 1
}

// The `[` that opens the link text ending at `before`'s end, allowing
// brackets nested inside it.
fn opening_bracket(before: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in before.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' if depth == 0 => return Some(i),
            '[' => depth -= 1,
            '\n' if before[..i].ends_with('\n') => return None,
            _ => {}
        }
    }
    None
}
