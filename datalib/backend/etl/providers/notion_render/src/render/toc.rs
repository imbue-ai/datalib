//! The source's table of contents: one document laying out every page
//! the store holds, under the pages and databases Notion keeps it in.
//!
//! `tree` holds the hierarchy, and no position among siblings, so a level
//! reads A to Z rather than in Notion's sidebar order. A database or a
//! page this source does not mirror has no document of its own: its entry
//! links Notion and carries the anchor the path above a page links to.

use std::collections::{HashMap, HashSet};

use datalib_etl_notion::ingest::db::TreeRow;
use datalib_etl_notion::ingest::schema_raw::TreeKind;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::html::escape_md_inline;
use datalib_etl_render::inputs::{Bucket, Input};
use datalib_etl_render::title::Title;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::ids;
use super::render::{notion_url, parent_of, RENDER_VERSION};

/// The bucket key. Every other Notion bucket is a Notion UUID.
pub const BUCKET: &str = "table_of_contents";
pub const TITLE: &str = "Notion TOC: Table-of-Contents (Datalib generated)";

/// The name a `tree` row goes by in a path or in the contents, or `None`
/// for a column or a toggle, which is walked through and not shown.
pub(super) fn shown_title(row: &TreeRow) -> Option<&str> {
    let title = row.title.as_deref().filter(|t| !t.trim().is_empty());
    match row.kind {
        Some(TreeKind::Page) => Some(title.unwrap_or("Untitled")),
        _ => title,
    }
}

/// Where the contents lists a `tree` row that has no document of its own.
pub(super) fn entry_href(stanza: &str, id: &str) -> String {
    let toc = ids::toc(stanza).uuid;
    let entry = ids::toc_entry(stanza, id).uuid;
    format!("/chat/{toc}#{entry}")
}

struct Entry<'a> {
    id: &'a str,
    row: &'a TreeRow,
    title: &'a str,
    children: Vec<Entry<'a>>,
}

struct Tree<'a> {
    rows: &'a HashMap<String, TreeRow>,
    children: HashMap<&'a str, Vec<&'a str>>,
}

fn is_held(row: &TreeRow) -> bool {
    row.kind == Some(TreeKind::Page)
}

impl<'a> Tree<'a> {
    /// The entries for `ids`, A to Z. A row that is not shown hands its
    /// children up, and a row with no held page at or below it is left
    /// out: in roots mode `tree` names pages all over the workspace.
    fn entries(&self, ids: &[&'a str], seen: &mut HashSet<&'a str>) -> Vec<Entry<'a>> {
        let mut out: Vec<Entry<'a>> = Vec::new();
        for &id in ids {
            if !seen.insert(id) {
                continue;
            }
            let Some(row) = self.rows.get(id) else {
                continue;
            };
            let below = self.children.get(id).map_or(&[][..], Vec::as_slice);
            let children = self.entries(below, seen);
            match shown_title(row) {
                None => out.extend(children),
                Some(title) if is_held(row) || !children.is_empty() => out.push(Entry {
                    id,
                    row,
                    title,
                    children,
                }),
                Some(_) => {}
            }
        }
        out.sort_by_cached_key(|e| (e.title.to_lowercase(), e.id));
        out
    }
}

/// Every held page, placed. A page whose parents loop never reaches the
/// top, so one still unplaced after the walk from the top starts its own
/// branch rather than going missing.
fn entries(rows: &HashMap<String, TreeRow>) -> Vec<Entry<'_>> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut top: Vec<&str> = Vec::new();
    for (id, row) in rows {
        match parent_of(row).filter(|p| rows.contains_key(p)) {
            Some(parent) => {
                let (parent, _) = rows.get_key_value(&parent).expect("just looked up");
                children.entry(parent).or_default().push(id);
            }
            None => top.push(id),
        }
    }
    let tree = Tree { rows, children };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = tree.entries(&top, &mut seen);
    let mut held: Vec<&str> = rows
        .iter()
        .filter(|(_, row)| is_held(row))
        .map(|(id, _)| id.as_str())
        .collect();
    held.sort_unstable();
    for id in held {
        out.extend(tree.entries(&[id], &mut seen));
    }
    out.sort_by_cached_key(|e| (e.title.to_lowercase(), e.id));
    out
}

fn write_entries(
    entries: &[Entry<'_>],
    depth: usize,
    stanza: &str,
    trashed: &HashSet<String>,
    out: &mut String,
) {
    for e in entries {
        out.push_str(&"  ".repeat(depth));
        out.push_str("- ");
        let title = escape_md_inline(e.title);
        if is_held(e.row) {
            let uuid = ids::page(stanza, e.id, e.row.created_time.as_deref()).uuid;
            out.push_str(&format!("[{title}](/chat/{uuid})"));
        } else {
            // A span, because a list item cannot sit inside the `<div>`
            // other sections use; the viewer highlights either.
            let anchor = ids::toc_entry(stanza, e.id).uuid;
            out.push_str(&format!(
                "<span id=\"m-{anchor}\" data-section-uuid=\"{anchor}\">[{title}]({})</span>",
                notion_url(e.id)
            ));
        }
        if trashed.contains(e.id) {
            out.push_str(" *(in Notion's trash)*");
        }
        out.push('\n');
        write_entries(&e.children, depth + 1, stanza, trashed, out);
    }
}

/// The earliest and the latest `created_time` of the held pages listed:
/// the moments in this document, which `tree` can say without the row
/// moving each time a page is edited.
fn page_stamps(entries: &[Entry<'_>]) -> (Option<String>, Option<String>) {
    let mut stamps: Vec<&str> = Vec::new();
    let mut stack: Vec<&Entry<'_>> = entries.iter().collect();
    while let Some(e) = stack.pop() {
        if is_held(e.row) {
            stamps.extend(e.row.created_time.as_deref());
        }
        stack.extend(&e.children);
    }
    let at = |s: Option<&&str>| s.map(|s| s.to_string());
    (at(stamps.iter().min()), at(stamps.iter().max()))
}

pub(super) struct TocDocument {
    pub markdown_uuid: String,
    pub markdown: String,
    pub rows: Vec<GridRow>,
    pub problems: Vec<ProblemRow>,
}

/// `None` when the store holds no page.
pub(super) fn document(
    tree: &HashMap<String, TreeRow>,
    trashed: &HashSet<String>,
    stanza: &str,
) -> Option<TocDocument> {
    let entries = entries(tree);
    if entries.is_empty() {
        return None;
    }
    let id = ids::toc(stanza);
    let mut markdown = String::from("---\n");
    markdown.push_str(&format!("title: {}\n", yaml_scalar(TITLE)));
    markdown.push_str(&format!("provider: {}\n", Provider::Notion));
    markdown.push_str(&format!("markdown_uuid: {}\n", id.uuid));
    markdown.push_str("source_label: Notion\n---\n\n");
    markdown.push_str(
        &Title {
            text: TITLE,
            markdown_uuid: Some(&id.uuid),
            suffix: None,
            source_url: None,
        }
        .render(),
    );
    markdown.push_str(
        "Every page this source holds, under the pages and databases Notion keeps it in, \
         A to Z at each level. A page links its document here; anything else links Notion.\n\n",
    );
    write_entries(&entries, 0, stanza, trashed, &mut markdown);

    let (created_at, modified_at) = page_stamps(&entries);
    let mut problems: Vec<ProblemRow> = Vec::new();
    let rows: Vec<GridRow> = GridRow::builder()
        .uuid(id.uuid.clone())
        .provider(Provider::Notion)
        .kind("Notion Table of Contents")
        .source_label("Notion")
        .is_document(true)
        .item_count(Some(0))
        .created_at(created_at)
        .modified_at(modified_at)
        .conversation_name(Some(TITLE.to_string()))
        .conversation_uuid(id.uuid.clone())
        .entire_chat(format!("/chat/{}", id.uuid))
        .body(TITLE.to_string())
        .upstream_id(Some(id.natural_key))
        .upstream_entity_kind(Some(id.entity_kind.to_string()))
        .markdown_uuid(Some(id.uuid.clone()))
        .build_or_record(stanza, BUCKET, RENDER_VERSION, &mut problems)
        .into_iter()
        .collect();
    Some(TocDocument {
        markdown_uuid: id.uuid,
        markdown,
        rows,
        problems,
    })
}

/// A rename, a move or a new page changes `tree`; a page entering the
/// trash changes only `pages`. Reading `pages` whole runs this again
/// after every page edit, which costs one pass over `tree` and writes the
/// same bytes, so nothing downstream sees a change.
pub(super) fn bucket() -> Bucket {
    Bucket {
        key: BUCKET.to_string(),
        inputs: vec![Input::whole_table("tree"), Input::whole_table("pages")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: TreeKind, title: Option<&str>, parent: Option<(&str, &str)>) -> TreeRow {
        TreeRow {
            kind: Some(kind),
            title: title.map(String::from),
            parent_type: parent.map(|p| p.0.into()),
            parent_id: parent.map(|p| p.1.into()),
            created_time: None,
        }
    }

    fn tree<const N: usize>(rows: [(&str, TreeRow); N]) -> HashMap<String, TreeRow> {
        rows.into_iter()
            .map(|(id, row)| (id.to_string(), row))
            .collect()
    }

    fn list(tree: &HashMap<String, TreeRow>, trashed: &[&str]) -> String {
        let trashed = trashed.iter().map(|s| s.to_string()).collect();
        let doc = document(tree, &trashed, "notion").expect("a held page");
        let (_, list) = doc
            .markdown
            .split_once("anything else links Notion.\n\n")
            .expect("the list follows the opening line");
        list.to_string()
    }

    fn page_link(title: &str, id: &str) -> String {
        format!("[{title}](/chat/{})", ids::page("notion", id, None).uuid)
    }

    fn database_entry(title: &str, id: &str) -> String {
        let anchor = ids::toc_entry("notion", id).uuid;
        format!(
            "<span id=\"m-{anchor}\" data-section-uuid=\"{anchor}\">\
             [{title}](https://www.notion.so/{id})</span>"
        )
    }

    /// The tree as a nested list: a column hands its children up, a
    /// database sits between a page and its rows, and each level reads A
    /// to Z whatever the case of its titles.
    #[test]
    fn pages_nest_under_their_parents_and_databases_a_to_z() {
        let t = tree([
            (
                "bridge",
                node(TreeKind::Page, Some("Bridge"), Some(("workspace", "w"))),
            ),
            (
                "armory",
                node(TreeKind::Page, Some("armory"), Some(("workspace", "w"))),
            ),
            (
                "col",
                node(TreeKind::Block, None, Some(("page_id", "bridge"))),
            ),
            (
                "db",
                node(TreeKind::Database, Some("Crew"), Some(("block_id", "col"))),
            ),
            (
                "riker",
                node(
                    TreeKind::Page,
                    Some("Riker"),
                    Some(("data_source_id", "db")),
                ),
            ),
            (
                "picard",
                node(
                    TreeKind::Page,
                    Some("Picard"),
                    Some(("data_source_id", "db")),
                ),
            ),
        ]);
        let want = format!(
            "- {}\n- {}\n  - {}\n    - {}\n    - {}\n",
            page_link("armory", "armory"),
            page_link("Bridge", "bridge"),
            database_entry("Crew", "db"),
            page_link("Picard", "picard"),
            page_link("Riker", "riker"),
        );
        assert_eq!(list(&t, &[]), want);
    }

    /// In roots mode `tree` names pages all over the workspace. Only what
    /// leads to a held page belongs here, or the contents of one root
    /// would list the whole workspace.
    #[test]
    fn a_branch_with_no_held_page_is_left_out() {
        let t = tree([
            (
                "wiki",
                node(
                    TreeKind::UnmirroredPage,
                    Some("Wiki"),
                    Some(("workspace", "w")),
                ),
            ),
            (
                "debrief",
                node(TreeKind::Page, Some("Debrief"), Some(("page_id", "wiki"))),
            ),
            (
                "elsewhere",
                node(
                    TreeKind::UnmirroredPage,
                    Some("Elsewhere"),
                    Some(("workspace", "w")),
                ),
            ),
            (
                "empty-db",
                node(TreeKind::Database, Some("Empty"), Some(("page_id", "wiki"))),
            ),
        ]);
        assert_eq!(
            list(&t, &[]),
            format!(
                "- {}\n  - {}\n",
                database_entry("Wiki", "wiki"),
                page_link("Debrief", "debrief")
            )
        );
    }

    /// A page shared with this credential sits under a parent it cannot
    /// see, and a parent the store has no row for places nothing. Both
    /// pages are at the top rather than lost.
    #[test]
    fn a_page_whose_parent_is_unknown_is_at_the_top() {
        let t = tree([
            (
                "hidden",
                TreeRow {
                    kind: Some(TreeKind::Block),
                    title: None,
                    parent_type: None,
                    parent_id: None,
                    created_time: None,
                },
            ),
            (
                "away",
                node(
                    TreeKind::Page,
                    Some("Away team"),
                    Some(("page_id", "hidden")),
                ),
            ),
            (
                "log",
                node(TreeKind::Page, Some("Log"), Some(("page_id", "no-row"))),
            ),
        ]);
        assert_eq!(
            list(&t, &[]),
            format!(
                "- {}\n- {}\n",
                page_link("Away team", "away"),
                page_link("Log", "log")
            )
        );
    }

    /// Parents can loop while a move is half seen. The pages in the loop
    /// never reach the top, and they are still listed, each once.
    #[test]
    fn pages_whose_parents_loop_are_still_listed_once() {
        let t = tree([
            (
                "a",
                node(TreeKind::Page, Some("Alpha"), Some(("page_id", "b"))),
            ),
            (
                "b",
                node(TreeKind::Page, Some("Beta"), Some(("page_id", "a"))),
            ),
            (
                "self",
                node(TreeKind::Page, Some("Self"), Some(("page_id", "self"))),
            ),
        ]);
        assert_eq!(
            list(&t, &[]),
            format!(
                "- {}\n  - {}\n- {}\n",
                page_link("Alpha", "a"),
                page_link("Beta", "b"),
                page_link("Self", "self")
            )
        );
    }

    /// A trashed page stays in the mirror, so the list has to say which
    /// entries Notion no longer shows.
    #[test]
    fn a_trashed_page_says_so() {
        let t = tree([
            ("kept", node(TreeKind::Page, Some("Kept"), None)),
            ("gone", node(TreeKind::Page, Some("Gone"), None)),
        ]);
        assert_eq!(
            list(&t, &["gone"]),
            format!(
                "- {} *(in Notion's trash)*\n- {}\n",
                page_link("Gone", "gone"),
                page_link("Kept", "kept")
            )
        );
    }

    /// A title is plain text upstream, so none of it may open a tag or
    /// end the link; a page with no title still gets a line to click.
    #[test]
    fn a_title_is_escaped_and_an_empty_one_is_named() {
        let t = tree([
            ("markup", node(TreeKind::Page, Some("<b>[x]</b>"), None)),
            ("blank", node(TreeKind::Page, Some(" "), None)),
        ]);
        let got = list(&t, &[]);
        assert!(got.contains("[&lt;b&gt;\\[x\\]&lt;/b&gt;](/chat/"), "{got}");
        assert!(got.contains("[Untitled](/chat/"), "{got}");
    }

    #[test]
    fn a_store_with_no_held_page_has_no_contents() {
        let t = tree([("wiki", node(TreeKind::UnmirroredPage, Some("Wiki"), None))]);
        assert!(document(&t, &HashSet::new(), "notion").is_none());
    }

    /// The grid row is how a person reaches the document, so it opens the
    /// document, carries the title, and adds nothing to the source's
    /// Items count. Its dates are the pages' own, so it sorts among them.
    #[test]
    fn the_row_opens_the_document_and_counts_no_item() {
        let mut older = node(TreeKind::Page, Some("Older"), None);
        older.created_time = Some("2369-01-01T00:00:00.000Z".into());
        let mut newer = node(TreeKind::Page, Some("Newer"), None);
        newer.created_time = Some("2370-01-01T00:00:00.000Z".into());
        let t = tree([("older", older), ("newer", newer)]);
        let doc = document(&t, &HashSet::new(), "notion").unwrap();
        assert!(doc.problems.is_empty(), "{:?}", doc.problems);
        let [row] = &doc.rows[..] else {
            panic!("one row: {:?}", doc.rows.len());
        };
        assert!(row.is_document);
        assert_eq!(row.conversation_name.as_deref(), Some(TITLE));
        assert_eq!(row.entire_chat, format!("/chat/{}", doc.markdown_uuid));
        assert_eq!(row.item_count, Some(0));
        assert_eq!(row.created_at.as_deref(), Some("2369-01-01T00:00:00.000Z"));
        assert_eq!(row.modified_at.as_deref(), Some("2370-01-01T00:00:00.000Z"));
    }
}
