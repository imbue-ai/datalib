//! Just enough of TOML's lexing to edit a config as text: where each
//! string, comment and bracket is, so a `[`, `#` or `,` inside a string is
//! never taken for one outside it. `config_order` cuts the file into
//! entries with it, and `config_array` edits an array where it stands.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A string of any of TOML's four kinds, quotes included. Only a
    /// multi-line one spans a line break.
    Str,
    /// `#` to the end of its line, the line break not included.
    Comment,
    /// One of `[ ] { } , =`.
    Punct,
    /// `\n` or `\r\n`.
    Newline,
    /// Spaces and tabs.
    Space,
    /// Anything else: a bare key, a number, a date, `true`.
    Bare,
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// Every token in `text`, in order, covering all of it.
pub fn tokens(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let newline_at = |i: usize| bytes[i] == b'\n' || bytes[i..].starts_with(b"\r\n");
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let kind = match bytes[i] {
            _ if newline_at(i) => {
                i += if bytes[i] == b'\r' { 2 } else { 1 };
                Kind::Newline
            }
            b' ' | b'\t' | b'\r' => {
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r') && !newline_at(i)
                {
                    i += 1;
                }
                Kind::Space
            }
            b'#' => {
                while i < bytes.len() && !newline_at(i) {
                    i += 1;
                }
                Kind::Comment
            }
            b'[' | b']' | b'{' | b'}' | b',' | b'=' => {
                i += 1;
                Kind::Punct
            }
            b'"' | b'\'' => {
                i = string_end(bytes, i);
                Kind::Str
            }
            _ => {
                while i < bytes.len() && !b" \t\r\n#[]{},=\"'".contains(&bytes[i]) {
                    i += 1;
                }
                Kind::Bare
            }
        };
        out.push(Token {
            kind,
            start,
            end: i,
        });
    }
    out
}

/// Past the closing quote of the string opening at `i`. A one-line string
/// left open ends at its line break.
fn string_end(bytes: &[u8], i: usize) -> usize {
    let q = bytes[i];
    let triple = bytes[i..].starts_with(&[q, q, q]);
    let mut j = i + if triple { 3 } else { 1 };
    while j < bytes.len() {
        if q == b'"' && bytes[j] == b'\\' {
            j += 2;
        } else if triple && bytes[j..].starts_with(&[q, q, q]) {
            // Up to two more quotes are the string's own last characters.
            let mut end = j + 3;
            while end < bytes.len() && end < j + 5 && bytes[end] == q {
                end += 1;
            }
            return end;
        } else if !triple && bytes[j] == q {
            return j + 1;
        } else if !triple && bytes[j] == b'\n' {
            return j;
        } else {
            j += 1;
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(Kind, &str)> {
        tokens(text)
            .iter()
            .map(|t| (t.kind, t.text(text)))
            .collect()
    }

    #[test]
    fn a_bracket_or_hash_inside_a_string_is_part_of_it() {
        use Kind::*;
        assert_eq!(
            kinds("a = [\"x]#\", 'y,z'] # c\r\n"),
            [
                (Bare, "a"),
                (Space, " "),
                (Punct, "="),
                (Space, " "),
                (Punct, "["),
                (Str, "\"x]#\""),
                (Punct, ","),
                (Space, " "),
                (Str, "'y,z'"),
                (Punct, "]"),
                (Space, " "),
                (Comment, "# c"),
                (Newline, "\r\n"),
            ]
        );
    }

    #[test]
    fn a_multi_line_string_is_one_token() {
        let text = "c = \"\"\"\n[not a header] \\\"\"\"\n\"\"\"\"\nx = 1";
        let strs: Vec<&str> = tokens(text)
            .iter()
            .filter(|t| t.kind == Kind::Str)
            .map(|t| t.text(text))
            .collect();
        assert_eq!(strs, ["\"\"\"\n[not a header] \\\"\"\"\n\"\"\"\""]);
        let literal = "c = '''a\n'b''''";
        assert_eq!(
            tokens(literal).last().unwrap().text(literal),
            "'''a\n'b''''"
        );
    }

    #[test]
    fn the_tokens_cover_the_text() {
        let text = "[[steps]] # s\n\tinputs = [ \"a\\\"\" , 1 ]\r\n'open\n";
        let joined: String = tokens(text).iter().map(|t| t.text(text)).collect();
        assert_eq!(joined, text);
    }
}
