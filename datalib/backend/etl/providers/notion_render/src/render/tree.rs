//! Where a page and a comment thread sit: the pages and databases above
//! a page, each a container. A page is read together, a subpage on its
//! own, and is a place, as it is in Notion's sidebar, so the pages under
//! pages are a tree to browse; a database is a place. A column or a toggle is walked
//! through, as the path above a page walks it. The raw store keeps no
//! record of the workspace, so the top of what it knows is the top here.

use std::collections::{HashMap, HashSet};

use datalib_etl_notion::ingest::db::TreeRow;
use datalib_etl_notion::ingest::schema_raw::TreeKind;
use datalib_etl_render::inputs::Lookup;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};

use super::ids;
use super::render::parent_of;
use super::toc::shown_title;

/// A page as a container: `created_time` is its own, as its uuid is
/// minted from it.
pub(super) struct PageNode<'a> {
    pub id: &'a str,
    pub created_time: Option<&'a str>,
    pub title: &'a str,
}

/// The containers of one document, and where its rows sit.
pub(super) struct Placed {
    pub data_nodes: Vec<DataNodeRow>,
    /// Where the page's own row sits.
    pub page: Lineage,
    /// Inside the page.
    pub inside: Lineage,
}

/// The page and every page and database above it, top first.
pub(super) fn page(
    md: &str,
    stanza: &str,
    page: &PageNode<'_>,
    tree: Lookup<'_, HashMap<String, TreeRow>>,
) -> Placed {
    let mut data_nodes: Vec<DataNodeRow> = above(md, stanza, page.id, tree);
    let mut at = Lineage::top();
    for node in &mut data_nodes {
        *node = node.clone().under(&at);
        at = at.below(node);
    }
    let own = DataNodeRow::new(
        md,
        ids::page(stanza, page.id, page.created_time).uuid,
        stanza,
        "Notion page",
    )
    .named(Some(page.title.to_string()))
    .upstream(ids::KIND_PAGE, page.id, None)
    .read_together()
    .place()
    .under(&at);
    let inside = at.below(&own);
    data_nodes.push(own);
    Placed {
        data_nodes,
        page: at,
        inside,
    }
}

/// A comment thread as a container of its page, read on its own.
pub(super) fn thread(md: &str, stanza: &str, discussion_id: &str, in_page: &Placed) -> DataNodeRow {
    DataNodeRow::new(
        md,
        ids::discussion(stanza, discussion_id).uuid,
        stanza,
        "Notion discussion",
    )
    .upstream(ids::KIND_DISCUSSION, discussion_id, None)
    .read_together()
    .under(&in_page.inside)
}

/// The pages and databases above `page_id`, top first, not yet placed.
fn above(
    md: &str,
    stanza: &str,
    page_id: &str,
    tree: Lookup<'_, HashMap<String, TreeRow>>,
) -> Vec<DataNodeRow> {
    let mut out: Vec<DataNodeRow> = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([page_id.to_string()]);
    let mut next = tree.get(page_id).and_then(parent_of);
    while let Some(id) = next {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(row) = tree.get(&id) else {
            break;
        };
        if let Some(node) = container(md, stanza, &id, row) {
            out.push(node);
        }
        next = parent_of(row);
    }
    out.reverse();
    out
}

fn container(md: &str, stanza: &str, id: &str, row: &TreeRow) -> Option<DataNodeRow> {
    let name = shown_title(row).map(String::from);
    match row.kind? {
        TreeKind::Page | TreeKind::UnmirroredPage => Some(
            DataNodeRow::new(
                md,
                ids::page(stanza, id, row.created_time.as_deref()).uuid,
                stanza,
                "Notion page",
            )
            .named(name)
            .upstream(ids::KIND_PAGE, id, None)
            .read_together()
            .place(),
        ),
        TreeKind::Database => Some(
            DataNodeRow::new(
                md,
                ids::database(stanza, id).uuid,
                stanza,
                "Notion database",
            )
            .named(name)
            .upstream(ids::KIND_DATABASE, id, None)
            .place(),
        ),
        TreeKind::Block => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::inputs::Inputs;

    fn row(kind: TreeKind, title: &str, parent: (&str, &str)) -> TreeRow {
        TreeRow {
            kind: Some(kind),
            title: Some(title.into()),
            parent_type: Some(parent.0.into()),
            parent_id: Some(parent.1.into()),
            created_time: Some("2364-01-01T00:00:00.000Z".into()),
        }
    }

    /// A page under a database under a page, with a column between: each
    /// page is read on its own and is a place, so is the database, the
    /// column is not there, and a thread sits inside its page.
    #[test]
    fn a_page_sits_under_its_pages_and_databases_and_a_thread_inside_it() {
        let tree: HashMap<String, TreeRow> = [
            (
                "top",
                row(TreeKind::Page, "Bridge", ("workspace", "workspace")),
            ),
            ("col", row(TreeKind::Block, "", ("page_id", "top"))),
            ("db", row(TreeKind::Database, "Logs", ("block_id", "col"))),
            (
                "p",
                row(TreeKind::Page, "Stardate 41153", ("database_id", "db")),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let inputs = Inputs::default();
        let at = page(
            "md",
            "notion",
            &PageNode {
                id: "p",
                created_time: None,
                title: "Stardate 41153",
            },
            inputs.lookup("tree", &tree),
        );
        let kinds: Vec<(&str, Option<&str>, bool, bool)> = at
            .data_nodes
            .iter()
            .map(|n| {
                (
                    n.kind.as_str(),
                    n.name.as_deref(),
                    n.read_together,
                    n.is_place,
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("Notion page", Some("Bridge"), true, true),
                ("Notion database", Some("Logs"), false, true),
                ("Notion page", Some("Stardate 41153"), true, true),
            ]
        );
        let [top, db, own] = &at.data_nodes[..] else {
            panic!("three containers")
        };
        assert_eq!(top.parent_uuid, None);
        assert_eq!(db.parent_uuid.as_deref(), Some(top.uuid.as_str()));
        assert_eq!(db.read_uuid.as_deref(), Some(top.uuid.as_str()));
        assert_eq!(own.parent_uuid.as_deref(), Some(db.uuid.as_str()));
        assert_eq!(own.place_uuid.as_deref(), Some(db.uuid.as_str()));
        assert_eq!(own.uuid, ids::page("notion", "p", None).uuid);
        assert_eq!(at.page.parent_uuid.as_deref(), Some(db.uuid.as_str()));
        assert_eq!(at.inside.read_uuid.as_deref(), Some(own.uuid.as_str()));

        let disc = thread("md", "notion", "d1", &at);
        assert_eq!(disc.parent_uuid.as_deref(), Some(own.uuid.as_str()));
        assert_eq!(disc.place_uuid.as_deref(), Some(own.uuid.as_str()));
        assert!(disc.read_together);
    }
}
