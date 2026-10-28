//! Which pages a roots-mode run mirrors: those whose chain of parents in
//! the tree reaches a root. Decided here over values; the run reads the
//! tree and fetches what this names.

use std::collections::{HashMap, HashSet};

use super::db::TreeRow;
use super::schema_raw::TreeKind;

/// The pages of `tree` under one of `roots`, a root page included: a
/// child page at any depth, a database's rows and what hangs off them,
/// a page inside a block of a page. A chain that leaves the tree, or
/// loops, ends without reaching a root.
pub fn pages_under(tree: &HashMap<String, TreeRow>, roots: &HashSet<String>) -> HashSet<String> {
    let mut known: HashMap<&str, bool> = HashMap::new();
    tree.iter()
        .filter(|(_, row)| is_page(row))
        .filter(|(id, _)| reaches_a_root(id, tree, roots, &mut known))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether a `tree` row is a page, mirrored or not. A body's link can
/// name a block or a database too, and Notion refuses either as a page.
pub fn is_page(row: &TreeRow) -> bool {
    matches!(
        row.kind,
        Some(TreeKind::Page) | Some(TreeKind::UnmirroredPage)
    )
}

fn reaches_a_root<'a>(
    start: &'a str,
    tree: &'a HashMap<String, TreeRow>,
    roots: &HashSet<String>,
    known: &mut HashMap<&'a str, bool>,
) -> bool {
    let mut chain: Vec<&'a str> = Vec::new();
    let mut seen: HashSet<&'a str> = HashSet::new();
    let mut at = Some(start);
    let answer = loop {
        let Some(id) = at else { break false };
        if let Some(&k) = known.get(id) {
            break k;
        }
        if roots.contains(id) {
            break true;
        }
        if !seen.insert(id) {
            break false;
        }
        chain.push(id);
        at = tree.get(id).and_then(parent_of);
    };
    for id in chain {
        known.insert(id, answer);
    }
    answer
}

fn parent_of(row: &TreeRow) -> Option<&str> {
    match row.parent_type.as_deref() {
        Some("workspace") | None => None,
        Some(_) => row.parent_id.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use TreeKind::{Block, Database, Page, UnmirroredPage};

    fn tree(rows: &[(&str, TreeKind, Option<&str>)]) -> HashMap<String, TreeRow> {
        rows.iter()
            .map(|(id, kind, parent)| {
                let row = TreeRow {
                    kind: Some(*kind),
                    title: None,
                    parent_type: Some(match parent {
                        Some(_) => "page_id".to_string(),
                        None => "workspace".to_string(),
                    }),
                    parent_id: parent.map(String::from),
                    created_time: None,
                };
                (id.to_string(), row)
            })
            .collect()
    }

    fn under(t: &HashMap<String, TreeRow>, roots: &[&str]) -> Vec<String> {
        let roots: HashSet<String> = roots.iter().map(|r| r.to_string()).collect();
        let mut out: Vec<String> = pages_under(t, &roots).into_iter().collect();
        out.sort();
        out
    }

    /// The case that sent this in: a database pasted as a root mirrored
    /// nothing, though its rows and their sub-pages are what was wanted.
    #[test]
    fn a_database_root_brings_its_rows_and_what_hangs_off_them() {
        let t = tree(&[
            ("missions", Database, None),
            ("away-team", UnmirroredPage, Some("missions")),
            ("debrief", UnmirroredPage, Some("away-team")),
            ("crew", Database, None),
            ("picard", UnmirroredPage, Some("crew")),
        ]);
        assert_eq!(under(&t, &["missions"]), ["away-team", "debrief"]);
    }

    /// A child page is found by its parent, whatever the root's body
    /// says, at any depth, and through a block (a toggle, a column) and a
    /// database inside a page; mirrored or not yet.
    #[test]
    fn a_page_root_brings_every_page_under_it() {
        let t = tree(&[
            ("bridge", Page, None),
            ("ops", Page, Some("bridge")),
            ("toggle", Block, Some("bridge")),
            ("column", Block, Some("toggle")),
            ("helm", UnmirroredPage, Some("column")),
            ("logs", Database, Some("ops")),
            ("stardate-1", UnmirroredPage, Some("logs")),
            ("sickbay", UnmirroredPage, None),
        ]);
        assert_eq!(
            under(&t, &["bridge"]),
            ["bridge", "helm", "ops", "stardate-1"]
        );
    }

    /// A parent the credential cannot see ends the chain, and so does a
    /// loop, rather than spinning.
    #[test]
    fn a_chain_that_leaves_the_tree_or_loops_reaches_nothing() {
        let t = tree(&[
            ("orphan", UnmirroredPage, Some("unseen")),
            ("a", Page, Some("b")),
            ("b", Page, Some("a")),
            ("loose-block", Block, None),
            ("in-loose", UnmirroredPage, Some("loose-block")),
        ]);
        assert!(under(&t, &["bridge"]).is_empty());
        assert_eq!(under(&t, &["a"]), ["a", "b"]);
    }

    /// One answer per chain is shared, so siblings deep under one root
    /// and pages outside it are told apart in one pass.
    #[test]
    fn siblings_share_the_answer_for_their_chain() {
        let t = tree(&[
            ("root", Page, None),
            ("mid", Page, Some("root")),
            ("leaf-1", UnmirroredPage, Some("mid")),
            ("leaf-2", UnmirroredPage, Some("mid")),
            ("other", Page, None),
            ("other-leaf", UnmirroredPage, Some("other")),
        ]);
        assert_eq!(under(&t, &["mid"]), ["leaf-1", "leaf-2", "mid"]);
    }
}
