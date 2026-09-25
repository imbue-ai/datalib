//! Two renders of one document, subtracted: the rows by uuid, the
//! sections by uuid, and a modified section's text word by word. What a
//! diff group writes is the `to` side with the result marked on it —
//! `diff_status` on every row, `diff-*` wrappers and `<ins>`/`<del>`
//! in the markdown — so a diff tree is an ordinary render tree that
//! happens to say how it differs from another.

use std::collections::{BTreeSet, HashMap};

use datalib_schema::diff_status::{DiffStatus, CHANGED_COLUMNS_SEPARATOR};
use datalib_schema::grid_rows::GridRow;
use similar::{capture_diff_slices, Algorithm, DiffOp};

use crate::section::Section;

/// One document's two sides subtracted.
#[derive(Debug, Clone, Default)]
pub struct DocumentDiff {
    /// The `to` side's rows with `diff_status` set, then the `from`
    /// side's rows that are gone, marked `removed`.
    pub rows: Vec<GridRow>,
    /// The `to` side's document with the changes marked; joined, it is
    /// the `.md` to write.
    pub sections: Vec<Section>,
    pub counts: Counts,
}

/// How many rows fell on each side of the comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub unchanged: usize,
}

impl Counts {
    pub fn changed(&self) -> usize {
        self.added + self.removed + self.modified
    }
}

/// A side that is absent is a document that does not exist at that
/// commit: everything on the other side is added, or removed.
pub fn diff_document(
    from: Option<(&[GridRow], &[Section])>,
    to: Option<(&[GridRow], &[Section])>,
) -> DocumentDiff {
    let (from_rows, from_sections) = from.unwrap_or((&[], &[]));
    let (to_rows, to_sections) = to.unwrap_or((&[], &[]));
    let (rows, counts) = diff_rows(from_rows, to_rows);
    DocumentDiff {
        rows,
        sections: diff_sections(from_sections, to_sections),
        counts,
    }
}

/// Rows keyed by `uuid`: in `to` only is added, in `from` only is
/// removed, in both with any cell different is modified — the differing
/// columns named — otherwise unchanged. The result keeps `to`'s order,
/// then the removed rows in `from`'s.
pub fn diff_rows(from: &[GridRow], to: &[GridRow]) -> (Vec<GridRow>, Counts) {
    let mut counts = Counts::default();
    let from_by_uuid: HashMap<&str, &GridRow> = from.iter().map(|r| (r.uuid.as_str(), r)).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::with_capacity(from.len() + to.len());
    for row in to {
        seen.insert(row.uuid.as_str());
        let (status, changed) = match from_by_uuid.get(row.uuid.as_str()) {
            None => {
                counts.added += 1;
                (DiffStatus::Added, None)
            }
            Some(before) => {
                let changed = changed_columns(before, row);
                if changed.is_empty() {
                    counts.unchanged += 1;
                    (DiffStatus::Unchanged, None)
                } else {
                    counts.modified += 1;
                    let joined = changed
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(&CHANGED_COLUMNS_SEPARATOR.to_string());
                    (DiffStatus::Modified, Some(joined))
                }
            }
        };
        out.push(marked(row, status, changed));
    }
    for row in from {
        if !seen.contains(row.uuid.as_str()) {
            counts.removed += 1;
            out.push(marked(row, DiffStatus::Removed, None));
        }
    }
    (out, counts)
}

fn marked(row: &GridRow, status: DiffStatus, changed: Option<String>) -> GridRow {
    GridRow {
        diff_status: Some(status.as_str().to_string()),
        diff_changed_columns: changed,
        ..row.clone()
    }
}

/// The columns whose value differs, by name, sorted. Compared through
/// the row's serialized form so a column added to `GridRow` is compared
/// without anyone remembering to list it here; the two diff columns
/// themselves are left out, since they are what this writes.
fn changed_columns(before: &GridRow, after: &GridRow) -> Vec<String> {
    let before = serde_json::to_value(before).expect("GridRow serializes");
    let after = serde_json::to_value(after).expect("GridRow serializes");
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return Vec::new();
    };
    before
        .iter()
        .filter(|(k, _)| k.as_str() != "diff_status" && k.as_str() != "diff_changed_columns")
        .filter(|(k, v)| after.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .collect()
}

const ADDED_OPEN: &str = "<div class=\"diff-added\">\n\n";
const REMOVED_OPEN: &str = "<div class=\"diff-removed\">\n\n";
const MODIFIED_OPEN: &str = "<div class=\"diff-modified\">\n\n";
const WRAP_CLOSE: &str = "</div>\n\n";

/// The `to` document with every keyed section marked against `from`:
/// one only in `to` is wrapped as added, one only in `from` is put
/// back where it was and wrapped as removed, one on both sides whose
/// bytes differ is wrapped as modified with the words that changed
/// marked inside. An unkeyed section — frontmatter, a `<details>`
/// wrapper — is structure rather than content and comes through from
/// `to` as it is.
pub fn diff_sections(from: &[Section], to: &[Section]) -> Vec<Section> {
    if to.is_empty() {
        // No `to` side at all: the document is gone, and what is shown
        // is `from`'s, its structure included, every section removed.
        return from
            .iter()
            .map(|s| removed(s).unwrap_or_else(|| s.clone()))
            .collect();
    }
    let from_keys: Vec<Key<'_>> = from.iter().map(Key::of).collect();
    let to_keys: Vec<Key<'_>> = to.iter().map(Key::of).collect();
    let mut out = Vec::with_capacity(to.len() + from.len());
    for op in capture_diff_slices(Algorithm::Myers, &from_keys, &to_keys) {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for i in 0..len {
                    let (before, after) = (&from[old_index + i], &to[new_index + i]);
                    out.push(match &after.uuid {
                        Some(uuid) if before.md != after.md => Section::keyed(
                            uuid,
                            format!(
                                "{MODIFIED_OPEN}{}{WRAP_CLOSE}",
                                inline_diff(&before.md, &after.md)
                            ),
                        ),
                        _ => after.clone(),
                    });
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                out.extend(
                    from[old_index..old_index + old_len]
                        .iter()
                        .filter_map(removed),
                );
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                out.extend(to[new_index..new_index + new_len].iter().map(added));
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                out.extend(
                    from[old_index..old_index + old_len]
                        .iter()
                        .filter_map(removed),
                );
                out.extend(to[new_index..new_index + new_len].iter().map(added));
            }
        }
    }
    out
}

/// What aligns the two section lists: a keyed section by its uuid, so
/// the same message on both sides lines up whatever its bytes; an
/// unkeyed one by its bytes, so an unchanged wrapper lines up and a
/// changed one is replaced by `to`'s.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key<'a> {
    Uuid(&'a str),
    Bytes(&'a str),
}

impl<'a> Key<'a> {
    fn of(s: &'a Section) -> Self {
        match &s.uuid {
            Some(uuid) => Key::Uuid(uuid),
            None => Key::Bytes(&s.md),
        }
    }
}

fn added(s: &Section) -> Section {
    match &s.uuid {
        Some(uuid) => Section::keyed(uuid, format!("{ADDED_OPEN}{}{WRAP_CLOSE}", s.md)),
        None => s.clone(),
    }
}

/// `None` for an unkeyed section: structure `from` had and `to` does
/// not is not shown, `to`'s own structure is.
fn removed(s: &Section) -> Option<Section> {
    s.uuid
        .as_ref()
        .map(|uuid| Section::keyed(uuid, format!("{REMOVED_OPEN}{}{WRAP_CLOSE}", s.md)))
}

/// `to` with what differs from `from` marked: lines first, then the
/// words inside a line that changed. A whole inserted line has its
/// content wrapped in `<ins>`, a whole deleted line is put back with its
/// content in `<del>`, and a line that changed gets the words that did
/// marked. A marker never crosses a table cell boundary or a line end,
/// never contains an HTML tag, and leaves a line's markdown prefix
/// (`## `, `- `, `> `) and a table's delimiter row alone, so a diff of a
/// heading is still a heading and a diff of a table is still a table.
pub fn inline_diff(from: &str, to: &str) -> String {
    let from_lines: Vec<&str> = from.split_inclusive('\n').collect();
    let to_lines: Vec<&str> = to.split_inclusive('\n').collect();
    let mut out = String::with_capacity(to.len() + from.len() / 4);
    for op in capture_diff_slices(Algorithm::Myers, &from_lines, &to_lines) {
        match op {
            DiffOp::Equal { new_index, len, .. } => {
                out.extend(to_lines[new_index..new_index + len].iter().copied())
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for line in &to_lines[new_index..new_index + new_len] {
                    mark_line(&mut out, "ins", line);
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for line in &from_lines[old_index..old_index + old_len] {
                    mark_line(&mut out, "del", line);
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let old = &from_lines[old_index..old_index + old_len];
                let new = &to_lines[new_index..new_index + new_len];
                let paired = old.len().min(new.len());
                for i in 0..paired {
                    word_diff(&mut out, old[i], new[i]);
                }
                for line in &old[paired..] {
                    mark_line(&mut out, "del", line);
                }
                for line in &new[paired..] {
                    mark_line(&mut out, "ins", line);
                }
            }
        }
    }
    out
}

/// One line, its every cell's content marked.
fn mark_line(out: &mut String, tag: &str, line: &str) {
    mark(out, tag, &tokens(line));
}

/// A changed line: the tokens that differ, marked.
fn word_diff(out: &mut String, from: &str, to: &str) {
    let from_tokens = tokens(from);
    let to_tokens = tokens(to);
    for op in capture_diff_slices(Algorithm::Myers, &from_tokens, &to_tokens) {
        match op {
            DiffOp::Equal { new_index, len, .. } => {
                out.extend(to_tokens[new_index..new_index + len].iter().copied())
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => mark(out, "del", &from_tokens[old_index..old_index + old_len]),
            DiffOp::Insert {
                new_index, new_len, ..
            } => mark(out, "ins", &to_tokens[new_index..new_index + new_len]),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                mark(out, "del", &from_tokens[old_index..old_index + old_len]);
                mark(out, "ins", &to_tokens[new_index..new_index + new_len]);
            }
        }
    }
}

/// Emit `run` with its content inside `<tag>`, closing the marker
/// before anything a marker may not contain — a cell boundary, a line
/// end, an HTML tag, a markdown prefix, a table delimiter — and
/// reopening it after. Whitespace rides along inside an open marker
/// and outside a closed one.
fn mark(out: &mut String, tag: &str, run: &[&str]) {
    let mut open = false;
    let mut at_line_start = out.is_empty() || out.ends_with('\n');
    // Whitespace after marked content is held until the next token says
    // whether it sits inside the marker (more content follows) or after
    // it (a boundary follows).
    let mut held = String::new();
    let close = |out: &mut String, open: &mut bool, held: &mut String| {
        if *open {
            out.push_str(&format!("</{tag}>"));
            *open = false;
        }
        out.push_str(held);
        held.clear();
    };
    for token in run {
        let structural = *token == "|"
            || token.starts_with('<')
            || is_table_rule(token)
            || (at_line_start && is_markdown_prefix(token));
        if token.contains('\n') || structural {
            close(out, &mut open, &mut held);
            out.push_str(token);
            at_line_start = token.ends_with('\n');
            continue;
        }
        if token.trim().is_empty() {
            if open {
                held.push_str(token);
            } else {
                out.push_str(token);
            }
            continue;
        }
        if !open {
            out.push_str(&format!("<{tag}>"));
            open = true;
        }
        out.push_str(&held);
        held.clear();
        out.push_str(token);
        at_line_start = false;
    }
    close(out, &mut open, &mut held);
}

/// `---`, `:---:` — a cell of a table's delimiter row.
fn is_table_rule(token: &str) -> bool {
    token.len() >= 3 && token.chars().all(|c| c == '-' || c == ':')
}

/// What a line may start with and still be what it is — a heading, a
/// list item, a quote.
fn is_markdown_prefix(token: &str) -> bool {
    matches!(
        token,
        "#" | "##" | "###" | "####" | "#####" | "######" | "-" | "*" | "+" | ">"
    ) || (token.ends_with('.') && token[..token.len() - 1].chars().all(|c| c.is_ascii_digit()))
}

/// HTML tags whole, `|` alone, words whole, whitespace runs whole.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let end = if c == b'<' {
            match s[i..].find('>') {
                Some(n) => i + n + 1,
                None => i + 1,
            }
        } else if c == b'|' {
            i + 1
        } else if c.is_ascii_whitespace() {
            i + s[i..]
                .find(|ch: char| !ch.is_ascii_whitespace())
                .unwrap_or(s.len() - i)
        } else {
            i + s[i..]
                .find(|ch: char| ch.is_ascii_whitespace() || ch == '<' || ch == '|')
                .unwrap_or(s.len() - i)
        };
        out.push(&s[start..end]);
        start = end;
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section::join;
    use datalib_schema::providers::Provider;

    fn row(uuid: &str, text: &str, author: &str) -> GridRow {
        GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Message")
            .source_label("Test")
            .conversation_uuid("chat")
            .entire_chat("/chat/chat")
            .body(text)
            .author(Some(author.to_string()))
            .build()
            .unwrap()
    }

    fn status(r: &GridRow) -> &str {
        r.diff_status.as_deref().unwrap()
    }

    #[test]
    fn rows_fall_on_every_side_of_the_table() {
        let from = vec![
            row("kept", "same", "Picard"),
            row("edited", "before", "Riker"),
            row("gone", "bye", "Data"),
        ];
        let to = vec![
            row("kept", "same", "Picard"),
            row("edited", "after", "Riker"),
            row("new", "hi", "Worf"),
        ];
        let (rows, counts) = diff_rows(&from, &to);
        let got: Vec<(&str, &str, Option<&str>)> = rows
            .iter()
            .map(|r| {
                (
                    r.uuid.as_str(),
                    status(r),
                    r.diff_changed_columns.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("kept", "unchanged", None),
                ("edited", "modified", Some("content_hash|preview")),
                ("new", "added", None),
                ("gone", "removed", None),
            ]
        );
        assert_eq!(
            counts,
            Counts {
                added: 1,
                removed: 1,
                modified: 1,
                unchanged: 1
            }
        );
        assert_eq!(rows[3].preview, "bye", "a removed row is the from side's");
    }

    #[test]
    fn several_changed_columns_are_named_sorted() {
        let from = vec![row("m", "before", "Riker")];
        let to = vec![row("m", "after", "William Riker")];
        let (rows, _) = diff_rows(&from, &to);
        assert_eq!(
            rows[0].diff_changed_columns.as_deref(),
            Some("author|content_hash|preview")
        );
    }

    #[test]
    fn a_missing_side_is_all_added_or_all_removed() {
        let rows = vec![row("a", "x", "A")];
        let sections = vec![Section::keyed("a", "<div>a</div>\n\n".into())];
        let d = diff_document(None, Some((&rows, &sections)));
        assert_eq!(status(&d.rows[0]), "added");
        assert!(join(&d.sections).starts_with(ADDED_OPEN));
        let d = diff_document(Some((&rows, &sections)), None);
        assert_eq!(status(&d.rows[0]), "removed");
        assert!(join(&d.sections).starts_with(REMOVED_OPEN));
    }

    #[test]
    fn sections_are_wrapped_by_fate_and_a_removed_one_keeps_its_place() {
        let front = Section::unkeyed("---\ntitle: t\n---\n\n".into());
        let a = Section::keyed("a", "<div id=\"m-a\">\n\nfirst\n\n</div>\n\n".into());
        let b = Section::keyed("b", "<div id=\"m-b\">\n\nsecond\n\n</div>\n\n".into());
        let b2 = Section::keyed(
            "b",
            "<div id=\"m-b\">\n\nsecond, edited\n\n</div>\n\n".into(),
        );
        let c = Section::keyed("c", "<div id=\"m-c\">\n\nthird\n\n</div>\n\n".into());
        let from = vec![front.clone(), a.clone(), b, c.clone()];
        let to = vec![front.clone(), b2, c.clone()];
        let out = diff_sections(&from, &to);
        let keys: Vec<Option<&str>> = out.iter().map(|s| s.uuid.as_deref()).collect();
        assert_eq!(keys, vec![None, Some("a"), Some("b"), Some("c")]);
        assert_eq!(out[0], front, "frontmatter passes through");
        assert_eq!(out[1].md, format!("{REMOVED_OPEN}{}{WRAP_CLOSE}", a.md));
        assert!(out[2].md.starts_with(MODIFIED_OPEN), "{}", out[2].md);
        assert!(
            out[2]
                .md
                .contains("<del>second</del><ins>second, edited</ins>"),
            "{}",
            out[2].md
        );
        assert_eq!(out[3], c, "an unchanged section is verbatim");
    }

    #[test]
    fn an_added_section_is_wrapped_and_changed_structure_is_the_to_sides() {
        let open_2 = Section::unkeyed("<details>\n<summary>2 tool steps</summary>\n\n".into());
        let open_3 = Section::unkeyed("<details>\n<summary>3 tool steps</summary>\n\n".into());
        let close = Section::unkeyed("</details>\n\n".into());
        let t1 = Section::keyed("t1", "<div id=\"m-t1\">\n\none\n\n</div>\n\n".into());
        let t2 = Section::keyed("t2", "<div id=\"m-t2\">\n\ntwo\n\n</div>\n\n".into());
        let t3 = Section::keyed("t3", "<div id=\"m-t3\">\n\nthree\n\n</div>\n\n".into());
        let from = vec![open_2, t1.clone(), t2.clone(), close.clone()];
        let to = vec![
            open_3.clone(),
            t1.clone(),
            t2.clone(),
            t3.clone(),
            close.clone(),
        ];
        let out = diff_sections(&from, &to);
        assert_eq!(
            out.iter().map(|s| s.uuid.as_deref()).collect::<Vec<_>>(),
            vec![None, Some("t1"), Some("t2"), Some("t3"), None]
        );
        assert_eq!(out[0], open_3, "the to side's wrapper, unmarked");
        assert_eq!(out[1], t1);
        assert_eq!(out[3].md, format!("{ADDED_OPEN}{}{WRAP_CLOSE}", t3.md));
        assert_eq!(out[4], close);
    }

    #[test]
    fn inline_diff_marks_words_and_never_splits_a_tag() {
        let from =
            "## <span class=\"a\">Picard</span> <time datetime=\"1\">then</time>\n\nMake it so.\n";
        let to = "## <span class=\"a\">Picard</span> <time datetime=\"2\">now</time>\n\nMake it so, Number One.\n";
        let out = inline_diff(from, to);
        assert_eq!(
            out,
            "## <span class=\"a\">Picard</span> <time datetime=\"1\"><del>then</del><time datetime=\"2\"><ins>now</ins></time>\n\n\
             Make it <del>so.</del><ins>so, Number One.</ins>\n"
        );
    }

    #[test]
    fn whole_lines_are_marked_in_place_and_a_heading_stays_a_heading() {
        assert_eq!(
            inline_diff("a\n", "a\nb c\n## d\n"),
            "a\n<ins>b c</ins>\n## <ins>d</ins>\n"
        );
        assert_eq!(
            inline_diff("a\n- b\nc\n", "a\n"),
            "a\n- <del>b</del>\n<del>c</del>\n"
        );
    }

    #[test]
    fn a_table_stays_a_table() {
        let from = "| Field | Value |\n| --- | --- |\n| Org | NCC-1701-D |\n| Phone | 1 |\n";
        let to =
            "| Field | Value |\n| --- | --- |\n| Org | NCC-1701-E |\n| Phone | 1 |\n| Home | 2 |\n";
        assert_eq!(
            inline_diff(from, to),
            "| Field | Value |\n| --- | --- |\n| Org | <del>NCC-1701-D</del><ins>NCC-1701-E</ins> |\n\
             | Phone | 1 |\n| <ins>Home</ins> | <ins>2</ins> |\n"
        );
        let gone = inline_diff(to, from);
        assert!(
            gone.ends_with("| <del>Home</del> | <del>2</del> |\n"),
            "{gone}"
        );
        // A delimiter row that appears whole is left as it is.
        let fresh = inline_diff("x\n", "x\n| A |\n| --- |\n| b |\n");
        assert_eq!(fresh, "x\n| <ins>A</ins> |\n| --- |\n| <ins>b</ins> |\n");
    }

    #[test]
    fn identical_text_is_returned_untouched() {
        let s = "<div id=\"m-x\">\n\n## hi\n\nthere\n\n</div>\n\n";
        assert_eq!(inline_diff(s, s), s);
    }

    #[test]
    fn a_document_gone_keeps_its_frontmatter() {
        let front = Section::unkeyed("---\ntitle: t\n---\n\n".into());
        let a = Section::keyed("a", "<div>a</div>\n\n".into());
        let out = diff_sections(&[front.clone(), a.clone()], &[]);
        assert_eq!(out[0], front);
        assert_eq!(out[1].md, format!("{REMOVED_OPEN}{}{WRAP_CLOSE}", a.md));
    }

    #[test]
    fn tokens_keep_tags_words_and_whitespace_whole() {
        assert_eq!(
            tokens("<a href=\"x y\">hi</a>  there|x\n"),
            vec![
                "<a href=\"x y\">",
                "hi",
                "</a>",
                "  ",
                "there",
                "|",
                "x",
                "\n"
            ]
        );
    }
}
