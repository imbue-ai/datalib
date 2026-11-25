//! Every key that starts with a prefix, as a range an index can seek:
//! `key >= prefix AND key < prefix_end(prefix)`. SQLite compares TEXT by
//! its UTF-8 bytes, and that order is the order of code points, so the
//! range holds exactly the keys that start with the prefix. `INSTR`,
//! `LIKE` and a bound `GLOB` give the same answer without the seek.

/// The least string greater than every string that starts with `prefix`,
/// or `None` when there is none (an empty prefix, or one of `char::MAX`
/// only), where the range has no upper end.
pub fn prefix_end(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        if let Some(next) = next_char(last) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

fn next_char(c: char) -> Option<char> {
    match c {
        char::MAX => None,
        '\u{D7FF}' => Some('\u{E000}'),
        _ => char::from_u32(c as u32 + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::prefix_end;

    #[test]
    fn the_end_follows_every_key_under_the_prefix_and_nothing_else() {
        let end = prefix_end("file:a/").unwrap();
        assert_eq!(end, "file:a0");
        for under in ["file:a/", "file:a/x", "file:a/\u{10FFFF}"] {
            assert!(under >= "file:a/" && under < end.as_str(), "{under}");
        }
        for outside in ["file:a", "file:a0", "file:b/", "file:a.txt"] {
            assert!(
                !(outside >= "file:a/" && outside < end.as_str()),
                "{outside}"
            );
        }
    }

    #[test]
    fn a_last_char_with_no_successor_carries_into_the_one_before() {
        assert_eq!(prefix_end("a\u{10FFFF}").as_deref(), Some("b"));
        assert_eq!(prefix_end("a\u{D7FF}").as_deref(), Some("a\u{E000}"));
        assert_eq!(prefix_end("\u{10FFFF}"), None);
        assert_eq!(prefix_end(""), None);
    }
}
