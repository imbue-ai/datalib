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
//! Measured against a real workspace, asking for such a block by id tells
//! the rest. A `bookmark`, a `tweet`, a `miro` and an
//! `external_object_instance` answer with their URL. An `alias`, a `drive`
//! and an `ai_block` answer `unsupported`, which is real content lost. A
//! `button` and a `breadcrumb` hold nothing a reader would miss.

use std::collections::BTreeMap;

/// The kinds of block a page is no worse without, so neither a request
/// nor a row. Any kind not named here is asked for, and reported if the
/// answer is no use, so a kind Notion adds later is not lost quietly.
const LOSES_NOTHING: &[&str] = &["button", "breadcrumb"];

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

    pub fn loses_nothing(&self) -> bool {
        self.alt
            .as_deref()
            .is_some_and(|alt| LOSES_NOTHING.contains(&alt))
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

    /// Holes markdown cannot express, as `(block_id, block_type)`.
    pub fn permanent_holes(&self) -> Vec<(String, String)> {
        self.unresolved
            .iter()
            .filter_map(|u| u.alt.clone().map(|a| (u.block_id.clone(), a)))
            .collect()
    }

    /// The hole `id` is a database: its marker becomes the `<database>`
    /// link an untruncated body carries, and it is no longer a hole.
    pub fn link_database(&mut self, id: &str, title: &str) {
        self.unresolved.retain(|u| u.block_id != id);
        if let Some((at, len)) = self.marker_of(id) {
            let link = format!(
                "<database url=\"https://app.notion.com/p/{}\">{title}</database>",
                id.replace('-', "")
            );
            self.markdown.replace_range(at..at + len, &link);
        }
    }

    /// The hole `id` came back as `part`: its text goes where the marker
    /// is, and any hole of its own is now one of this body's.
    pub fn fill(&mut self, id: &str, part: PageBody) {
        self.put(id, part.markdown.trim_end_matches('\n'));
        for hole in part.unresolved {
            if !self.unresolved.iter().any(|u| u.block_id == hole.block_id) {
                self.unresolved.push(hole);
            }
        }
    }

    /// The block `id` is a link, which goes where the marker is.
    pub fn link_block(&mut self, id: &str, link: &str) {
        self.put(id, link);
    }

    fn put(&mut self, id: &str, text: &str) {
        self.unresolved.retain(|u| u.block_id != id);
        match self.marker_of(id) {
            Some((at, len)) => self.markdown.replace_range(at..at + len, text),
            None if text.is_empty() => {}
            None => {
                self.markdown.push('\n');
                self.markdown.push_str(text);
                self.markdown.push('\n');
            }
        }
    }

    fn marker_of(&self, id: &str) -> Option<(usize, usize)> {
        tags_at(&self.markdown, "<unknown")
            .into_iter()
            .find(|(_, tag)| attr(tag, "url").and_then(id_in_url).as_deref() == Some(id))
            .map(|(at, tag)| (at, tag.len()))
    }

    /// Whether any hole could hold something a reader would miss, and so
    /// whether the body is worth following further or reporting on.
    pub fn has_holes_that_matter(&self) -> bool {
        self.unresolved.iter().any(|u| !u.loses_nothing())
    }

    /// Whether a fetchable hole is still open, so text may be missing,
    /// and links with it.
    pub fn is_missing_text(&self) -> bool {
        self.fetchable_holes().next().is_some()
    }

    /// What is still missing from this body once everything that can be
    /// asked for has been, as a sentence for its `problems` row.
    pub fn what_is_lost(&self) -> Option<String> {
        let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
        for hole in self.unresolved.iter().filter(|u| !u.loses_nothing()) {
            *kinds
                .entry(hole.alt.as_deref().unwrap_or("subtree"))
                .or_default() += 1;
        }
        if kinds.is_empty() {
            return None;
        }
        let listed: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        Some(format!(
            "missing from this page because Notion will not give it: {}",
            listed.join(", ")
        ))
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

/// A block's URL as the link to put where its marker is, or `None` for a
/// block with no URL, or one that is not http or https: the URL came from
/// upstream, and what is written here gets drawn as a link.
pub fn link_of_block(block: &serde_json::Value) -> Option<String> {
    let content = block.get(block.get("type")?.as_str()?)?;
    let url = content.get("url")?.as_str()?;
    let scheme_is_web = url.starts_with("https://") || url.starts_with("http://");
    let would_end_the_link = url.contains(|c: char| c == '<' || c == '>' || c.is_whitespace());
    if !scheme_is_web || would_end_the_link {
        return None;
    }
    let caption: String = content
        .get("caption")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("plain_text")?.as_str())
        .collect();
    let text = if caption.trim().is_empty() {
        url
    } else {
        caption.trim()
    };
    let text = text.replace('[', "\\[").replace(']', "\\]");
    Some(format!("[{text}](<{url}>)"))
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

    /// A truncated body's hole can be an inline database, which Notion
    /// will not return as a page of markdown; its marker is the link an
    /// untruncated body would have carried.
    #[test]
    fn a_database_hole_becomes_a_database_link() {
        let mut body = parse(&json!({"truncated": true, "markdown":
            "Duty roster: <unknown url=\"https://app.notion.com/p/x#253a550faf95817babfeee3c8c58dec5\"/>\n\
             <unknown url=\"https://app.notion.com/p/x#348a550faf95803780d8c19ae5c2c9cd\"/>\n"}));
        body.link_database("253a550f-af95-817b-abfe-ee3c8c58dec5", "Crew");
        assert_eq!(
            body.markdown,
            "Duty roster: <database url=\"https://app.notion.com/p/253a550faf95817babfeee3c8c58dec5\">Crew</database>\n\
             <unknown url=\"https://app.notion.com/p/x#348a550faf95803780d8c19ae5c2c9cd\"/>\n"
        );
        let holes: Vec<_> = body.fetchable_holes().map(|u| &u.block_id).collect();
        assert_eq!(holes, vec!["348a550f-af95-8037-80d8-c19ae5c2c9cd"]);
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
