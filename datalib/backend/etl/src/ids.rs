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
            normalize_id_token("fb3f094f-2bef-4b62-8137-1383b4f3a1e4"),
            "fb3f094f-2bef-4b62-8137-1383b4f3a1e4"
        );
        assert_eq!(
            normalize_id_token("f9a3f309bde54852944042374cc01dc5"),
            "f9a3f309-bde5-4852-9440-42374cc01dc5"
        );
        // A token that only holds a uuid is left as written.
        assert_eq!(
            normalize_id_token("g-p-f9a3f309bde54852944042374cc01dc5"),
            "g-p-f9a3f309bde54852944042374cc01dc5"
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
            "f9a3f309-bde5-4852-9440-42374cc01dc5"
        );
        assert_eq!(
            normalize_id_token("https://www.notion.so/f9a3f309bde54852944042374cc01dc5#h1"),
            "f9a3f309-bde5-4852-9440-42374cc01dc5"
        );
    }
}
