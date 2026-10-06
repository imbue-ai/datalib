//! Edit a TOML array of strings where it stands, keeping how it was
//! written: on one line, or one id per line with its indentation, its
//! trailing comma and the comments and blank lines between the ids. The
//! result is refused unless it reads back as the ids asked for. The UI
//! edits a fan-in's `inputs` the same way (`datalib/ui/src/config/tomlText.ts`).

use crate::config_lex::{tokens, Kind};

enum Entry {
    Id {
        /// As written, quotes and all.
        token: String,
        /// The string it spells; `None` for anything that is not one.
        value: Option<String>,
        /// The whitespace before it when it starts a line; `None` when it
        /// follows something else on its line.
        indent: Option<String>,
        /// A comment after it on its line, with the space before the `#`.
        note: String,
    },
    Comment {
        indent: String,
        text: String,
    },
    Blank,
}

struct Scanned {
    entries: Vec<Entry>,
    /// A comment on the `[` line, with the space before it.
    head: String,
    multiline: bool,
    trailing_comma: bool,
    /// The indentation of a `]` on its own line; `None` when it closes the
    /// last line of entries.
    close: Option<String>,
}

/// `written` is one whole array, `[` to `]`, as the file has it. The
/// strings of `after` that it already holds keep their place, and the
/// rest go at the end. Unchanged text when the strings do not change.
pub fn edit_string_array(written: &str, after: &[String]) -> Result<String, String> {
    let scanned = scan(written)?;
    let before: Vec<&str> = scanned.entries.iter().filter_map(value).collect();
    if before == after {
        return Ok(written.to_string());
    }
    let added: Vec<Entry> = after
        .iter()
        .filter(|v| !before.contains(&v.as_str()))
        .map(|v| Entry::Id {
            token: toml::Value::String(v.clone()).to_string(),
            value: Some(v.clone()),
            indent: None,
            note: String::new(),
        })
        .collect();
    let kept = scanned
        .entries
        .iter()
        .filter(|e| value(e).is_none_or(|v| after.iter().any(|a| a == v)));
    let entries: Vec<&Entry> = kept.chain(&added).collect();
    let edited = render(&scanned, &entries);
    let wanted: Vec<&str> = entries.iter().copied().filter_map(value).collect();
    if strings_in(&edited).is_none_or(|got| got != wanted) {
        return Err(format!(
            "the edited array would not read back as {wanted:?}; left as it is:\n{edited}"
        ));
    }
    Ok(edited)
}

fn value(e: &Entry) -> Option<&str> {
    match e {
        Entry::Id { value, .. } => value.as_deref(),
        _ => None,
    }
}

/// The strings of the array `text` holds, as TOML reads them.
fn strings_in(text: &str) -> Option<Vec<String>> {
    let table: toml::Table = toml::from_str(&format!("v = {text}")).ok()?;
    let array = table.get("v")?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

fn scan(written: &str) -> Result<Scanned, String> {
    let mut s = Scanned {
        entries: Vec::new(),
        head: String::new(),
        multiline: false,
        trailing_comma: false,
        close: None,
    };
    let tokens = tokens(written);
    let mut rest = tokens.iter();
    if rest.next().map(|t| t.text(written)) != Some("[") {
        return Err(format!("not an array: {written:?}"));
    }
    // Only whitespace so far on a line after the first.
    let mut line_start = false;
    let mut space = String::new();
    // The index of the id on the current line, for a note after it.
    let mut on_this_line: Option<usize> = None;
    for t in rest {
        let text = t.text(written);
        match (t.kind, text) {
            (Kind::Punct, "]") => {
                s.close = line_start.then_some(space);
                return Ok(s);
            }
            (Kind::Punct, ",") => s.trailing_comma = true,
            (Kind::Punct, _) => return Err(format!("{text:?} in an array of strings")),
            (Kind::Space, _) => {
                space.push_str(text);
                continue;
            }
            (Kind::Newline, _) => {
                if line_start {
                    s.entries.push(Entry::Blank);
                }
                s.multiline = true;
                line_start = true;
                space.clear();
                on_this_line = None;
                continue;
            }
            (Kind::Comment, _) => match on_this_line {
                Some(at) => {
                    if let Entry::Id { note, .. } = &mut s.entries[at] {
                        *note = format!("{space}{text}");
                    }
                }
                None if !s.multiline => s.head = format!("{space}{text}"),
                None => s.entries.push(Entry::Comment {
                    indent: space.clone(),
                    text: text.to_string(),
                }),
            },
            (Kind::Str | Kind::Bare, _) => {
                on_this_line = Some(s.entries.len());
                s.entries.push(Entry::Id {
                    token: text.to_string(),
                    value: strings_in(&format!("[{text}]")).and_then(|v| v.into_iter().next()),
                    indent: line_start.then(|| space.clone()),
                    note: String::new(),
                });
                s.trailing_comma = false;
            }
        }
        line_start = false;
        space.clear();
    }
    Err(format!("the array is not closed: {written:?}"))
}

fn render(s: &Scanned, entries: &[&Entry]) -> String {
    let tokens: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Id { token, .. } => Some(token.as_str()),
            _ => None,
        })
        .collect();
    if !s.multiline {
        return format!("[{}]", tokens.join(", "));
    }
    let had_ids = s.entries.iter().any(|e| matches!(e, Entry::Id { .. }));
    let trailing_comma = !had_ids || s.trailing_comma;
    let indent = s
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::Id { indent, .. } => indent.clone(),
            Entry::Comment { indent, .. } => Some(indent.clone()),
            Entry::Blank => None,
        })
        .unwrap_or_else(|| format!("{}  ", s.close.as_deref().unwrap_or("")));
    let mut ids_left = tokens.len();
    let lines: Vec<String> = entries
        .iter()
        .map(|e| match e {
            Entry::Blank => String::new(),
            Entry::Comment { indent, text } => format!("{indent}{text}"),
            Entry::Id {
                token,
                indent: own,
                note,
                ..
            } => {
                ids_left -= 1;
                let comma = if ids_left > 0 || trailing_comma {
                    ","
                } else {
                    ""
                };
                format!("{}{token}{comma}{note}", own.as_deref().unwrap_or(&indent))
            }
        })
        .collect();
    let open = format!("[{}\n", s.head);
    // A `]` after a comment would be part of it.
    let close_inline = s.close.is_none()
        && matches!(entries.last(), Some(Entry::Id { note, .. }) if note.is_empty());
    if close_inline {
        return format!("{open}{}]", lines.join("\n"));
    }
    let close = format!("{}]", s.close.as_deref().unwrap_or(""));
    if lines.is_empty() {
        format!("{open}{close}")
    } else {
        format!("{open}{}\n{close}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(written: &str, after: &[&str]) -> String {
        let after: Vec<String> = after.iter().map(|s| s.to_string()).collect();
        edit_string_array(written, &after).unwrap()
    }

    /// #897: the qmd upgrade wrote every `inputs` it touched on one line.
    #[test]
    fn one_id_per_line_stays_one_id_per_line() {
        let written = "[\n    \"a/render_markdown\",\n    \"b/render_markdown\",\n]";
        assert_eq!(
            edit(written, &["a/keyword_index", "b/keyword_index"]),
            "[\n    \"a/keyword_index\",\n    \"b/keyword_index\",\n]"
        );
        assert_eq!(
            edit(written, &["a/render_markdown"]),
            "[\n    \"a/render_markdown\",\n]"
        );
    }

    #[test]
    fn no_trailing_comma_stays_without_one() {
        let written = "[\n  \"a\",\n  \"b\"\n]";
        assert_eq!(
            edit(written, &["a", "b", "c"]),
            "[\n  \"a\",\n  \"b\",\n  \"c\"\n]"
        );
        assert_eq!(edit(written, &["b"]), "[\n  \"b\"\n]");
    }

    #[test]
    fn comments_and_blank_lines_survive() {
        let written =
            "[ # every source\n  # the crew\n  \"a\", # logs [old]\n\n  \"b\",\n  # \"c\",\n]";
        assert_eq!(
            edit(written, &["b", "d"]),
            "[ # every source\n  # the crew\n\n  \"b\",\n  # \"c\",\n  \"d\",\n]"
        );
        assert_eq!(
            edit(written, &["a", "b", "d"]),
            "[ # every source\n  # the crew\n  \"a\", # logs [old]\n\n  \"b\",\n  # \"c\",\n  \"d\",\n]"
        );
    }

    #[test]
    fn one_line_stays_one_line() {
        assert_eq!(edit("[\"a\", 'b']", &["b", "c"]), "['b', \"c\"]");
        assert_eq!(edit("[]", &["a"]), "[\"a\"]");
    }

    #[test]
    fn an_unchanged_array_is_left_as_written() {
        let written = "[ \"a\" ,'b' ]";
        assert_eq!(edit(written, &["a", "b"]), written);
    }

    #[test]
    fn a_close_after_the_last_id_stays_there() {
        assert_eq!(edit("[\n  \"a\",\n  \"b\"]", &["a"]), "[\n  \"a\"]");
        // …unless a comment would swallow it.
        assert_eq!(edit("[\n  # x\n  \"a\"]", &[]), "[\n  # x\n]");
    }

    /// Anything the edit cannot lay out is refused, not written.
    #[test]
    fn what_it_cannot_edit_is_refused() {
        let after = ["a".to_string()];
        for written in ["[\"a\", [\"b\"]]", "[\"a\"", "\"a\""] {
            assert!(edit_string_array(written, &after).is_err(), "{written}");
        }
    }
}
