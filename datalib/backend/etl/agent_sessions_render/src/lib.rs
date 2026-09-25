//! What the agent-session render crates (claude_code, codex) share: the
//! item a transcript line becomes, the folded block a run of tool
//! traffic becomes, and the text helpers both read their JSON with.

use datalib_etl_chat_common::types::{ItemKind, NormalizedChatItem, UpstreamRef};
use datalib_id::Identity;
use serde_json::Value;

/// One item of a transcript, as chat-common renders it.
pub fn item(
    id: Identity,
    author_id: &str,
    author_display: String,
    date_ms: Option<i64>,
    text: String,
    kind_label: &str,
    is_aside: bool,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_id: author_id.to_string(),
        author_display,
        date_ms,
        text: (!text.trim().is_empty()).then_some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside,
        unread: false,
        problems: Vec::new(),
    }
}

/// A collapsed block: `summary` shows, `body` opens.
pub fn details(summary: &str, body: &str) -> String {
    if body.trim().is_empty() {
        format!("<details><summary>{summary}</summary>\n\n</details>")
    } else {
        format!("<details><summary>{summary}</summary>\n\n{body}\n\n</details>")
    }
}

pub fn fenced(s: &str) -> String {
    if s.trim().is_empty() {
        return String::new();
    }
    // A fence longer than any run of backticks in the body, so a tool
    // result that itself contains ``` cannot close it early.
    let longest = s.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}", s.trim_end())
}

/// At most `max_bytes` of `s`, cut on a char boundary, with a line
/// saying how much was left out and how to see it.
pub fn clamp(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n… [{} more bytes not shown; raise max_tool_result_bytes and re-render to see them]",
        &s[..end],
        s.len() - end
    )
}

/// The last path component of the working directory: what a person
/// would call the project.
pub fn project_of(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub fn iso_to_ms(s: &str) -> Option<i64> {
    datalib_time::parse_strict(s)
        .ok()
        .map(|t| t.to_unix_millis())
}

pub fn json_is_empty(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

/// `v` with every object's keys sorted, so it serializes the same way
/// whatever order the agent wrote them in.
pub fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(pairs.len());
            for (k, val) in pairs {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_tool_results_are_cut_and_say_so() {
        let s = "x".repeat(100);
        let out = clamp(&s, 10);
        assert!(out.starts_with("xxxxxxxxxx\n"));
        assert!(out.contains("90 more bytes"));
        assert_eq!(clamp("short", 10), "short");
        // Cut lands on a char boundary.
        let e = "é".repeat(10);
        assert!(clamp(&e, 3).starts_with("é\n"));
    }

    #[test]
    fn a_fence_outlasts_backticks_in_the_body() {
        let f = fenced("a\n```\nb");
        assert!(f.starts_with("````\n"), "{f}");
        assert!(f.ends_with("\n````"), "{f}");
    }

    #[test]
    fn project_is_the_last_path_component() {
        assert_eq!(project_of("/Users/picard/src/enterprise"), "enterprise");
        assert_eq!(project_of("/Users/picard/src/enterprise/"), "enterprise");
        assert_eq!(project_of("/"), "/");
    }
}
