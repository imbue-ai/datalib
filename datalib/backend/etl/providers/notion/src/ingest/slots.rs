//! Turning Notion's rotating file URLs into stable slots.
//!
//! Notion hands out pre-signed links for every hosted file and mints a
//! fresh signature on each fetch, valid about an hour. Two fetches of an
//! unchanged page therefore return different bytes, differing only in
//! `X-Amz-Signature` and friends. Stored verbatim that makes every page
//! with an attachment look modified on every run, which defeats
//! incremental render.
//!
//! The stable part is the unsigned URL — scheme, host and path. That is
//! the *slot*: it names the attachment across every re-signing, and it
//! is what the stored markdown and the CAS edge are both keyed on.
//!
//! The rewrite has to be narrow. Real workspace markdown is full of
//! ordinary links whose query strings carry meaning (a measured page
//! held 1,548 links with live query strings against 28 Notion
//! attachments). Stripping queries indiscriminately would corrupt them,
//! so a URL is only rewritten when it is BOTH on a Notion file host AND
//! carries a signature parameter.

/// Hosts Notion serves uploaded file bytes from.
fn is_notion_file_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h.starts_with("prod-files-secure.")
        || h == "file.notion.so"
        || h.ends_with(".notion-static.com")
        || h == "s3.us-west-2.amazonaws.com"
}

/// Query parameters that mark a link as pre-signed and therefore
/// short-lived.
fn has_signature(query: &str) -> bool {
    query.split('&').any(|kv| {
        let k = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
        k == "x-amz-signature" || k == "signature" || k == "expirationtimestamp"
    })
}

fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    Some((scheme, host, &rest[end..]))
}

/// The stable identity of an attachment: the URL with its query string
/// and fragment removed. Returns `None` for anything that is not a
/// signed Notion file link, which is the signal to leave it alone.
pub fn slot_of(url: &str) -> Option<String> {
    let (scheme, host, path_etc) = split_url(url)?;
    if !is_notion_file_host(host) {
        return None;
    }
    let (path, query) = match path_etc.split_once('?') {
        Some((p, q)) => (p, q.split('#').next().unwrap_or(q)),
        None => return None,
    };
    if !has_signature(query) {
        return None;
    }
    Some(format!("{scheme}://{host}{path}"))
}

/// Rewrite every signed Notion file URL in `markdown` to its slot.
/// Returns the rewritten text and the distinct slots found, in first
/// appearance order — the set of attachments this page owns.
pub fn rewrite(markdown: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(markdown.len());
    let mut slots: Vec<String> = Vec::new();
    let mut rest = markdown;
    while let Some(start) = find_url_start(rest) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>'))
            .unwrap_or(tail.len());
        let url = &tail[..end];
        match slot_of(url) {
            Some(slot) => {
                if !slots.iter().any(|s| s == &slot) {
                    slots.push(slot.clone());
                }
                out.push_str(&slot);
            }
            None => out.push_str(url),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    (out, slots)
}

fn find_url_start(s: &str) -> Option<usize> {
    let h = s.find("http://");
    let g = s.find("https://");
    match (h, g) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/7e787524-f8de-4371-b9da-13349dbe2bef/8b76f77e-046f-4914-aa3c-7039f7e98b4f/image.png?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ASIA%2F20260907&X-Amz-Signature=deadbeef";
    const SLOT: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/7e787524-f8de-4371-b9da-13349dbe2bef/8b76f77e-046f-4914-aa3c-7039f7e98b4f/image.png";

    #[test]
    fn signed_notion_file_reduces_to_its_slot() {
        assert_eq!(slot_of(SIGNED).as_deref(), Some(SLOT));
    }

    /// The regression that matters most: an unchanged page must
    /// serialize identically to itself. Two fetches differ only in the
    /// signature, so the rewrite has to collapse them.
    #[test]
    fn two_signings_of_one_file_rewrite_identically() {
        let a = format!("![]({SIGNED})");
        let b = a.replace("deadbeef", "cafef00d").replace("ASIA", "ASIB");
        assert_ne!(a, b, "fixtures must actually differ");
        assert_eq!(rewrite(&a).0, rewrite(&b).0);
    }

    /// A measured page carried 1,548 ordinary links with live query
    /// strings against 28 Notion attachments. Stripping queries
    /// indiscriminately would corrupt every one of them.
    #[test]
    fn ordinary_links_keep_their_query_strings() {
        for url in [
            "https://www.doordash.com/store/x?pickup=false&utm_campaign=abc",
            "https://docs.google.com/spreadsheets/d/1/edit?gid=0&usp=sharing",
            "https://en.wikipedia.org/wiki/Bandra",
        ] {
            assert_eq!(slot_of(url), None, "{url} must not be treated as a slot");
            assert_eq!(rewrite(url).0, url, "{url} must survive verbatim");
        }
    }

    /// Host alone is not enough — an unsigned S3 link keeps its query.
    #[test]
    fn notion_host_without_a_signature_is_left_alone() {
        let u = "https://prod-files-secure.s3.us-west-2.amazonaws.com/a/b/c.png?width=64";
        assert_eq!(slot_of(u), None);
    }

    #[test]
    fn rewrite_collects_distinct_slots_in_order() {
        let md = format!("![one]({SIGNED})\n\ntext\n\n![again]({SIGNED})");
        let (out, slots) = rewrite(&md);
        assert_eq!(slots, vec![SLOT.to_string()]);
        assert_eq!(out.matches(SLOT).count(), 2);
        assert!(!out.contains("X-Amz-Signature"));
    }

    #[test]
    fn markdown_without_attachments_is_returned_unchanged() {
        let md = "# Title\n\nSee [Bandra](https://en.wikipedia.org/wiki/Bandra).\n";
        let (out, slots) = rewrite(md);
        assert_eq!(out, md);
        assert!(slots.is_empty());
    }
}
