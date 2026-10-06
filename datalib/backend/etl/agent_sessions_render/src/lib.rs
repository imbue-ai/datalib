//! What the agent-session render crates (claude_code, codex) share: the
//! item a transcript line becomes, the folded block a run of tool
//! traffic becomes, and the text helpers both read their JSON with.

use datalib_etl_chat_common::normalize::json_pretty_sorted;
use datalib_etl_chat_common::types::{ItemKind, NormalizedChatItem, UpstreamRef};
use datalib_etl_render::html::{escape_text, md_code_block};
use datalib_id::Identity;
use serde_json::Value;

/// The `bucket_query` of an agent-session render's diff scan: the
/// transcripts the diff since `?1` touched, by `transcripts.id`, over
/// the two tables every agent-session raw store keeps.
pub const TRANSCRIPT_BUCKETS_SQL: &str = "
    SELECT DISTINCT bucket FROM (
        SELECT coalesce(to_transcript_id, from_transcript_id) AS bucket
          FROM dolt_diff_records
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        UNION
        SELECT coalesce(to_id, from_id)
          FROM dolt_diff_transcripts
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
    )
    WHERE bucket IS NOT NULL
";

/// One item of a transcript, as chat-common renders it.
pub fn item(
    id: Identity,
    author_display: String,
    date_ms: Option<i64>,
    text: String,
    kind_label: &str,
    is_aside: bool,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_handle: None,
        author_display,
        date_ms,
        text: (!text.trim().is_empty()).then_some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside,
        unread: false,
        recipients: Vec::new(),
        problems: Vec::new(),
    }
}

/// A collapsed block: `summary`, plain text, shows; `body`, markdown,
/// opens.
pub fn details(summary: &str, body: &str) -> String {
    let summary = escape_text(summary);
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
    md_code_block("", s.trim_end())
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

/// A tool's JSON input or arguments as a fenced block, keys sorted;
/// empty when there is nothing in it to show.
pub fn json_block(v: &Value, max_bytes: usize) -> String {
    if json_is_empty(v) {
        return String::new();
    }
    md_code_block("json", &clamp(&json_pretty_sorted(v), max_bytes))
}

fn json_is_empty(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool is named by whoever wrote it; its name cannot open a tag
    /// inside the summary.
    #[test]
    fn a_tool_named_in_markup_renders_escaped() {
        assert_eq!(
            details("Tool use: <script>x</script> & co", ""),
            "<details><summary>Tool use: &lt;script&gt;x&lt;/script&gt; &amp; co</summary>\n\n</details>"
        );
    }

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
