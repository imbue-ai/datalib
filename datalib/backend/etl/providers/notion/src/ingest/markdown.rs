//! Reading Notion's enhanced-markdown response.
//!
//! `GET /v1/pages/{id}/markdown` returns the page body already rendered,
//! plus two things the body alone doesn't tell you: whether it is
//! complete, and which pages it links.
//!
//! **`truncated` covers two different situations and the tell is one
//! attribute.** A hole always leaves a marker in the text:
//!
//! - `<unknown url="…#id"/>` with **no** `alt` is a subtree the response
//!   was too large to inline. Fetching that id as its own page of
//!   markdown returns it.
//! - `<unknown url="…#id" alt="button"/>` with an `alt` is a block type
//!   markdown cannot express. Fetching it returns a stub carrying the
//!   same id — an infinite regress. Never follow one.
//!
//! Measured against a real workspace: `button`, `alias` and `drive` all
//! appear as the permanent kind.

/// A hole in a page's markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub block_id: String,
    /// The block type, when Notion named one. `Some` means markdown
    /// cannot represent this block and a follow-up fetch will loop;
    /// `None` means the content exists and can be fetched.
    pub alt: Option<String>,
}

impl Unresolved {
    /// Whether fetching this id as a page of markdown will return
    /// content. False for block types markdown cannot express.
    pub fn is_fetchable(&self) -> bool {
        self.alt.is_none()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PageBody {
    pub markdown: String,
    pub truncated: bool,
    pub unresolved: Vec<Unresolved>,
    /// Pages the body links, as a child (`<page>`) or a mention
    /// (`<mention-page>`), in appearance order. A `<database>` link is
    /// not a page.
    pub linked_pages: Vec<String>,
}

impl PageBody {
    pub fn fetchable_holes(&self) -> impl Iterator<Item = &Unresolved> {
        self.unresolved.iter().filter(|u| u.is_fetchable())
    }

    /// Holes that will never resolve, as `(block_id, block_type)`. These
    /// belong in the rendered document's problem list: the page is
    /// genuinely incomplete and saying so is the point.
    pub fn permanent_holes(&self) -> Vec<(String, String)> {
        self.unresolved
            .iter()
            .filter_map(|u| u.alt.clone().map(|a| (u.block_id.clone(), a)))
            .collect()
    }
}

/// Dash-format a bare 32-hex Notion id. Returns the input unchanged if
/// it isn't 32 hex characters.
pub fn dash_uuid(s: &str) -> String {
    let raw: String = s.chars().filter(|c| *c != '-').collect();
    if raw.len() != 32 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return s.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    )
}

/// The last 32-hex run in a Notion URL, dashed. Notion page links carry
/// the id in the final path segment (`…/Title-<hex32>`), and an
/// `<unknown>` marker carries it in the fragment.
fn id_in_url(url: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let bytes = url.as_bytes();
    let mut run_start: Option<usize> = None;
    for i in 0..=bytes.len() {
        let is_hex = i < bytes.len() && (bytes[i] as char).is_ascii_hexdigit();
        match (is_hex, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(st)) => {
                if i - st == 32 {
                    best = Some(&url[st..i]);
                }
                run_start = None;
            }
            _ => {}
        }
    }
    best.map(dash_uuid)
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn tags<'a>(md: &'a str, open: &str) -> Vec<&'a str> {
    tags_at(md, open).into_iter().map(|(_, t)| t).collect()
}

/// Each tag opening with `open`, and where in `md` it starts.
fn tags_at<'a>(md: &'a str, open: &str) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = md[from..].find(open) {
        let at = from + i;
        match md[at..].find('>') {
            Some(j) => {
                out.push((at, &md[at..=at + j]));
                from = at + j + 1;
            }
            None => break,
        }
    }
    out
}

/// Read a `page_markdown` response body.
pub fn parse(resp: &serde_json::Value) -> PageBody {
    let markdown = resp
        .get("markdown")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let mut unresolved: Vec<Unresolved> = Vec::new();
    for tag in tags(&markdown, "<unknown") {
        let Some(id) = attr(tag, "url").and_then(id_in_url) else {
            continue;
        };
        unresolved.push(Unresolved {
            block_id: id,
            alt: attr(tag, "alt").map(|s| s.to_string()),
        });
    }
    // Ids the response listed but that left no marker in the text. Rare
    // — one in 1,387 on the largest page measured — but they are real
    // missing content, so they must not be inferred away from the body.
    if let Some(arr) = resp.get("unresolved_block_ids").and_then(|v| v.as_array()) {
        for v in arr.iter().filter_map(|v| v.as_str()) {
            let id = dash_uuid(v);
            if !unresolved.iter().any(|u| u.block_id == id) {
                unresolved.push(Unresolved {
                    block_id: id,
                    alt: None,
                });
            }
        }
    }

    let mut linked_pages: Vec<String> = Vec::new();
    let mut links: Vec<(usize, &str)> = ["<page ", "<mention-page "]
        .into_iter()
        .flat_map(|open| tags_at(&markdown, open))
        .collect();
    links.sort_by_key(|(at, _)| *at);
    for (_, t) in links {
        if let Some(id) = attr(t, "url").and_then(id_in_url) {
            if !linked_pages.contains(&id) {
                linked_pages.push(id);
            }
        }
    }

    PageBody {
        truncated: resp
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        unresolved,
        linked_pages,
        markdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dashes_a_bare_hex_id() {
        assert_eq!(
            dash_uuid("348a550faf9580a08973e679d9e1c6c9"),
            "348a550f-af95-80a0-8973-e679d9e1c6c9"
        );
        let already = "348a550f-af95-80a0-8973-e679d9e1c6c9";
        assert_eq!(dash_uuid(already), already);
        assert_eq!(dash_uuid("not-an-id"), "not-an-id");
    }

    /// Real shapes: `<page url="https://app.notion.com/p/Title-<hex32>">`
    /// and `<mention-page url="https://app.notion.com/p/<hex32>"/>`, read
    /// in the order they appear. A `<database>` link is not a page.
    #[test]
    fn linked_pages_are_read_from_child_and_mention_tags() {
        let body = parse(&json!({"markdown":
            "Ask <mention-page url=\"https://app.notion.com/p/1701d000000040008000000000000e01\"/> first.\n\
             <page url=\"https://app.notion.com/p/Standup-Notes-37ba550faf9580e7a583fc322a3ed287\">Standup Notes</page>\n\
             <database url=\"https://app.notion.com/p/Tasks-2e2a550faf9580498b0ecee9468fa146\">Tasks</database>\n\
             Again: <mention-page url=\"https://app.notion.com/p/1701d000000040008000000000000e01\"/>\n"}));
        assert_eq!(
            body.linked_pages,
            vec![
                "1701d000-0000-4000-8000-000000000e01",
                "37ba550f-af95-80e7-a583-fc322a3ed287"
            ]
        );
    }

    /// The distinction the whole module exists for. An `alt`-bearing
    /// hole is a block type markdown cannot express: following it
    /// returns a stub carrying the same id, so it must never be queued.
    #[test]
    fn alt_separates_permanent_holes_from_fetchable_ones() {
        let body = parse(&json!({"truncated": true, "markdown":
            "<unknown url=\"https://app.notion.com/p/x#253a550faf95817babfeee3c8c58dec5\"/>\n\
             <unknown url=\"https://app.notion.com/p/x#348a550faf95803780d8c19ae5c2c9cd\" alt=\"button\"/>\n"}));
        assert!(body.truncated);
        let fetchable: Vec<_> = body.fetchable_holes().map(|u| &u.block_id).collect();
        assert_eq!(fetchable, vec!["253a550f-af95-817b-abfe-ee3c8c58dec5"]);
        assert_eq!(
            body.permanent_holes(),
            vec![(
                "348a550f-af95-8037-80d8-c19ae5c2c9cd".to_string(),
                "button".to_string()
            )]
        );
    }

    /// An id listed in `unresolved_block_ids` with no marker in the text
    /// is still missing content. Inferring the hole set from the body
    /// alone would drop it silently.
    #[test]
    fn ids_without_a_marker_in_the_body_are_still_counted() {
        let body = parse(&json!({
            "truncated": true,
            "markdown": "just text, no markers\n",
            "unresolved_block_ids": ["253a550faf9581a4b80cfea90ee3c960"],
        }));
        assert_eq!(body.unresolved.len(), 1);
        assert!(body.unresolved[0].is_fetchable());
    }

    /// A database row with no body is the common case — 71% of pages in
    /// a measured workspace — and must not look like a failure.
    #[test]
    fn an_empty_body_parses_cleanly() {
        let body = parse(&json!({"markdown": "", "truncated": false}));
        assert!(body.markdown.is_empty());
        assert!(!body.truncated);
        assert!(body.unresolved.is_empty());
        assert!(body.linked_pages.is_empty());
    }
}
