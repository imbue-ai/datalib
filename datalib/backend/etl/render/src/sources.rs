//! The sources an answer cited, drawn as one numbered list after it.
//! Each provider reads its own citation shapes into [`Source`]s; how the
//! list looks is decided here, once.

use crate::html::{escape_md_inline, md_link_dest};

/// One cited page. `title` falls back to the url in the link text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source<'a> {
    pub url: &'a str,
    pub title: Option<&'a str>,
    pub site: Option<&'a str>,
}

/// `**Sources**` and a numbered list, each url once in order of first
/// citation; `None` when nothing was cited.
pub fn sources_list<'a>(sources: impl IntoIterator<Item = Source<'a>>) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<String> = sources
        .into_iter()
        .filter(|s| !s.url.is_empty() && seen.insert(s.url))
        .enumerate()
        .map(|(i, s)| {
            let mut line = format!(
                "{}. [{}]({})",
                i + 1,
                escape_md_inline(s.title.filter(|t| !t.is_empty()).unwrap_or(s.url)),
                md_link_dest(s.url)
            );
            if let Some(site) = s.site.filter(|t| !t.is_empty()) {
                line.push_str(&format!(" — {}", escape_md_inline(site)));
            }
            line
        })
        .collect();
    (!lines.is_empty()).then(|| format!("**Sources**\n\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_url_is_listed_once_with_its_title_and_site() {
        let a = Source {
            url: "https://memory-alpha.example/Risa",
            title: Some("Risa"),
            site: Some("Memory Alpha"),
        };
        let bare = Source {
            url: "https://example.com/[x]",
            title: None,
            site: None,
        };
        assert_eq!(
            sources_list([a, bare, a]).unwrap(),
            "**Sources**\n\n\
             1. [Risa](https://memory-alpha.example/Risa) — Memory Alpha\n\
             2. [https://example.com/\\[x\\]](https://example.com/[x])"
        );
        assert_eq!(sources_list([]), None);
    }
}
