//! What each bound parameter was, without its value: a value can be a
//! person's words or address, and a log line gets pasted into an issue.
//!
//! SQLite can hand back a statement's text with each parameter replaced by
//! its value (`sqlite3_expanded_sql`), copying everything else as written.
//! So the statement as written and the expanded one agree up to each
//! parameter, and walking the two side by side finds each value's literal.

/// The shape of each parameter's value, in the order they appear; `None`
/// when the two texts part ways somewhere this reader does not expect.
pub fn shapes(sql: &str, expanded: &str) -> Option<Vec<String>> {
    let (sql, expanded) = (sql.as_bytes(), expanded.as_bytes());
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < sql.len() {
        let skip = verbatim_len(&sql[i..]);
        if skip > 0 {
            if expanded.get(j..j + skip)? != &sql[i..i + skip] {
                return None;
            }
            i += skip;
            j += skip;
            continue;
        }
        if let Some(len) = parameter_len(&sql[i..]) {
            let (shape, used) = literal(&expanded[j..])?;
            out.push(shape);
            i += len;
            j += used;
            continue;
        }
        if expanded.get(j) != Some(&sql[i]) {
            return None;
        }
        i += 1;
        j += 1;
    }
    (j == expanded.len()).then_some(out)
}

/// A quoted string, quoted name or comment at the start of `s`, which
/// SQLite copies whole and in which a `?` is no parameter.
fn verbatim_len(s: &[u8]) -> usize {
    let closing = |open: usize, close: &[u8]| -> usize {
        let mut k = open;
        while k < s.len() {
            if s[k..].starts_with(close) {
                return k + close.len();
            }
            k += 1;
        }
        s.len()
    };
    match s {
        [q @ (b'\'' | b'"' | b'`'), ..] => {
            // A doubled quote inside is one character of the string.
            let mut k = 1;
            while k < s.len() {
                if s[k] == *q {
                    if s.get(k + 1) == Some(q) {
                        k += 2;
                        continue;
                    }
                    return k + 1;
                }
                k += 1;
            }
            s.len()
        }
        [b'[', ..] => closing(1, b"]"),
        [b'-', b'-', ..] => closing(2, b"\n"),
        [b'/', b'*', ..] => closing(2, b"*/"),
        _ => 0,
    }
}

/// `?`, `?NNN`, `:name`, `@name` or `$name` at the start of `s`.
fn parameter_len(s: &[u8]) -> Option<usize> {
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    match s.first()? {
        b'?' => Some(1 + s[1..].iter().take_while(|c| c.is_ascii_digit()).count()),
        b':' | b'@' | b'$' => {
            let n = s[1..].iter().take_while(|c| word(c)).count();
            (n > 0).then_some(1 + n)
        }
        _ => None,
    }
}

/// The literal SQLite wrote for one value, as its shape, and its length.
fn literal(s: &[u8]) -> Option<(String, usize)> {
    if s.starts_with(b"NULL") {
        return Some(("null".into(), 4));
    }
    if let [b'x' | b'X', b'\'', ..] = s {
        let len = verbatim_len(&s[1..]) + 1;
        return Some((format!("blob({} bytes)", (len - 3) / 2), len));
    }
    if s.first() == Some(&b'\'') {
        let len = verbatim_len(s);
        let text = String::from_utf8_lossy(&s[1..len - 1]).replace("''", "'");
        let shape = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Array(items)) => format!("json[{}]", items.len()),
            _ => format!("text({})", text.chars().count()),
        };
        return Some((shape, len));
    }
    let len = s
        .iter()
        .take_while(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E'))
        .count();
    if len == 0 {
        return None;
    }
    let real = s[..len].iter().any(|c| matches!(c, b'.' | b'e' | b'E'));
    Some((if real { "real" } else { "int" }.into(), len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_parameter_is_named_by_its_shape_not_its_value() {
        let sql = "SELECT * FROM t WHERE source_id = ?1 AND n > ? AND x IN (SELECT value FROM json_each(?3)) AND b = :b AND z IS ?";
        let expanded = "SELECT * FROM t WHERE source_id = 'linear-imbue' AND n > 42 AND x IN (SELECT value FROM json_each('[\"a\",\"b\",\"c\"]')) AND b = X'00ff' AND z IS NULL";
        assert_eq!(
            shapes(sql, expanded).unwrap(),
            ["text(12)", "int", "json[3]", "blob(2 bytes)", "null"]
        );
    }

    #[test]
    fn a_question_mark_in_a_string_or_comment_is_no_parameter() {
        let sql = "SELECT '?', \"a?\" -- why?\n FROM t /* ? */ WHERE k = '$seen' AND v = ?";
        let expanded =
            "SELECT '?', \"a?\" -- why?\n FROM t /* ? */ WHERE k = '$seen' AND v = 'it''s'";
        assert_eq!(shapes(sql, expanded).unwrap(), ["text(4)"]);
    }

    #[test]
    fn a_reused_numbered_parameter_is_named_each_time() {
        let sql = "SELECT 1 WHERE a = ?1 OR b = ?1";
        let expanded = "SELECT 1 WHERE a = -1.5 OR b = -1.5";
        assert_eq!(shapes(sql, expanded).unwrap(), ["real", "real"]);
    }

    #[test]
    fn texts_that_part_ways_have_no_shapes() {
        assert_eq!(shapes("SELECT ?", "SELECT 1 + 1"), None);
        assert_eq!(shapes("SELECT 1", "SELECT 2"), None);
    }
}
