//! Helpers for accepting either a bare provider ID or a copy-pasted
//! browser URL in user-facing config. Applied at the provider's `fetch`
//! entry — *not* at deserialize time — so logs and error messages
//! preserve the user's original input until the moment we actually
//! need a normalized token to hit the API.

pub fn normalize_id_token(s: &str) -> String {
    let trimmed = s.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return datalib_uuid_text::canonical(trimmed).unwrap_or_else(|| trimmed.to_string());
    }
    if let Some(id) = datalib_uuid_text::at_end(trimmed) {
        return id;
    }
    let no_frag = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let last = no_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(no_query);
    last.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_for_bare_ids() {
        assert_eq!(
            normalize_id_token("1701c000-a000-4000-8000-00000000c001"),
            "1701c000-a000-4000-8000-00000000c001"
        );
        assert_eq!(
            normalize_id_token("1701d000a0004000800000000000d007"),
            "1701d000-a000-4000-8000-00000000d007"
        );
        // A token that only holds a uuid is left as written.
        assert_eq!(
            normalize_id_token("g-p-1701d000a0004000800000000000d007"),
            "g-p-1701d000a0004000800000000000d007"
        );
    }

    #[test]
    fn strips_claude_url() {
        assert_eq!(
            normalize_id_token("https://claude.ai/chat/1701c000-a000-4000-8000-00000000c001"),
            "1701c000-a000-4000-8000-00000000c001"
        );
    }

    #[test]
    fn strips_chatgpt_url_with_query() {
        assert_eq!(
            normalize_id_token(
                "https://chatgpt.com/c/1701c000-a000-4000-8000-00000000c002?model=gpt-5"
            ),
            "1701c000-a000-4000-8000-00000000c002"
        );
    }

    #[test]
    fn strips_notion_url_with_title() {
        assert_eq!(
            normalize_id_token(
                "https://www.notion.so/myws/Roadmap-1701d000a0004000800000000000d007"
            ),
            "1701d000-a000-4000-8000-00000000d007"
        );
        assert_eq!(
            normalize_id_token("https://www.notion.so/1701d000a0004000800000000000d007#h1"),
            "1701d000-a000-4000-8000-00000000d007"
        );
    }
}
