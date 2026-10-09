//! The YAML front matter that opens every rendered document. Each
//! renderer writes its own keys; a value from upstream goes through
//! [`yaml_scalar`], so nothing in it can end its line or the block.

/// `s` as a YAML scalar, JSON-quoted: YAML reads a JSON string as a
/// double-quoted scalar, and in one a line break, a quote or a `---` is
/// an escape or plain text rather than structure.
pub fn yaml_scalar(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes to JSON")
}

#[cfg(test)]
mod tests {
    use super::yaml_scalar;

    #[test]
    fn a_value_cannot_leave_its_line() {
        assert_eq!(yaml_scalar("Bridge Crew"), "\"Bridge Crew\"");
        assert_eq!(
            yaml_scalar("a: b\n---\ntitle: \"x\" \\ #c"),
            "\"a: b\\n---\\ntitle: \\\"x\\\" \\\\ #c\""
        );
        assert_eq!(yaml_scalar(""), "\"\"");
        assert!(!yaml_scalar("\r\u{0}\t").contains(['\r', '\n', '\0']));
    }
}
