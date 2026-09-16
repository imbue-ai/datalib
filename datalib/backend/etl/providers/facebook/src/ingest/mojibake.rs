//! Undo the encoding bug in every Facebook JSON export: non-ASCII text
//! is written as one `\u00XX` escape per UTF-8 *byte*, so `✊` arrives
//! as `â` and a JSON parser hands back three latin-1
//! characters. Every string is checked, since a plain ASCII one is
//! unaffected either way.

use serde_json::Value;

/// Rewrite every string in `v`, in place, whose characters all fit in one
/// byte and which decodes as UTF-8 once those bytes are read as UTF-8.
pub fn fix(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(fixed) = fix_str(s) {
                *s = fixed;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(fix),
        Value::Object(map) => map.values_mut().for_each(fix),
        _ => {}
    }
}

/// `Some(decoded)` when `s` is mis-encoded, `None` when it is fine as it
/// is. A string with any character above U+00FF was never latin-1 bytes,
/// and one that is pure ASCII reads the same either way.
pub fn fix_str(s: &str) -> Option<String> {
    if s.is_ascii() || s.chars().any(|c| c as u32 > 0xFF) {
        return None;
    }
    let bytes: Vec<u8> = s.chars().map(|c| c as u32 as u8).collect();
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_the_byte_escapes_facebook_writes() {
        // What the export holds for "Project Data Liberation ✊".
        let raw = "Project Data Liberation \u{e2}\u{9c}\u{8a}";
        assert_eq!(fix_str(raw).as_deref(), Some("Project Data Liberation ✊"));
        // Two-byte sequences too (é).
        assert_eq!(fix_str("caf\u{c3}\u{a9}").as_deref(), Some("café"));
    }

    #[test]
    fn leaves_correct_text_alone() {
        assert_eq!(fix_str("plain ascii"), None);
        // Already-correct non-ASCII has a char above U+00FF, so it is
        // not a candidate.
        assert_eq!(fix_str("already ✊"), None);
        // Latin-1 that is not valid UTF-8 stays as it is.
        assert_eq!(fix_str("caf\u{e9}"), None);
    }

    #[test]
    fn walks_the_whole_document() {
        let mut v = json!({
            "title": "ok",
            "data": [{"post": "Liberation \u{e2}\u{9c}\u{8a}"}],
            "n": 3
        });
        fix(&mut v);
        assert_eq!(v["data"][0]["post"], "Liberation ✊");
        assert_eq!(v["title"], "ok");
    }
}
