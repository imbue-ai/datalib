//! Helpers for accepting either a bare provider ID or a copy-pasted
//! browser URL in user-facing config. Applied at the provider's `fetch`
//! entry — *not* at deserialize time — so logs and error messages
//! preserve the user's original input until the moment we actually
//! need a normalized token to hit the API.

/// Strip a paste-able URL down to its trailing id token.
///
/// - Not `http(s)://`: returned unchanged (trim only).
/// - http(s) URL: query/fragment dropped, then the last path segment
///   is examined. If it ends in a 32-hex run (Notion's `Title-<hex32>`
///   shape), return just that trailing token. Otherwise return the
///   whole last segment (covers `claude.ai/chat/<uuid>` and
///   `chatgpt.com/c/<uuid>` where the segment is already a dashed UUID).
///
/// The normalized string is fed unchanged to the provider; Notion's
/// `format_uuid` further expands undashed hex into dashed UUID form.
pub fn normalize_id_token(s: &str) -> String {
    let trimmed = s.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return trimmed.to_string();
    }
    let no_frag = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let last = no_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(no_query);
    if last.len() >= 32 {
        let tail = &last[last.len() - 32..];
        if tail.chars().all(|c| c.is_ascii_hexdigit()) {
            let before = last.as_bytes().get(last.len().wrapping_sub(33));
            if last.len() == 32 || before == Some(&b'-') {
                return tail.to_string();
            }
        }
    }
    last.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_for_bare_ids() {
        assert_eq!(
            normalize_id_token("fb3f094f-2bef-4b62-8137-1383b4f3a1e4"),
            "fb3f094f-2bef-4b62-8137-1383b4f3a1e4"
        );
        assert_eq!(
            normalize_id_token("f9a3f309bde54852944042374cc01dc5"),
            "f9a3f309bde54852944042374cc01dc5"
        );
    }

    #[test]
    fn strips_claude_url() {
        assert_eq!(
            normalize_id_token("https://claude.ai/chat/fb3f094f-2bef-4b62-8137-1383b4f3a1e4"),
            "fb3f094f-2bef-4b62-8137-1383b4f3a1e4"
        );
    }

    #[test]
    fn strips_chatgpt_url_with_query() {
        assert_eq!(
            normalize_id_token(
                "https://chatgpt.com/c/6a0de88e-d104-83ea-9f3a-dad7a9c734a8?model=gpt-5"
            ),
            "6a0de88e-d104-83ea-9f3a-dad7a9c734a8"
        );
    }

    #[test]
    fn strips_notion_url_with_title() {
        assert_eq!(
            normalize_id_token(
                "https://www.notion.so/myws/Roadmap-f9a3f309bde54852944042374cc01dc5"
            ),
            "f9a3f309bde54852944042374cc01dc5"
        );
        assert_eq!(
            normalize_id_token("https://www.notion.so/f9a3f309bde54852944042374cc01dc5#h1"),
            "f9a3f309bde54852944042374cc01dc5"
        );
    }
}
