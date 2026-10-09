//! A conversation whose prompts were edited or answers regenerated is a
//! tree. The page reads the branch the account last saw; every other
//! branch is kept, folded in just before the version shown where it
//! forked off. This decides that order; `render` only draws it.

use std::collections::{HashMap, HashSet};

/// One message as the provider has it: its id and its parent's. A
/// parent outside the set (a hidden root) makes the message a root.
#[derive(Debug, Clone, Copy)]
pub struct TreeNode<'a> {
    pub id: &'a str,
    pub parent: Option<&'a str>,
}

/// One message in reading order, and the branches it sits in, outermost
/// first: empty on the branch shown, else each enclosing branch's first
/// message id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed<'a> {
    pub id: &'a str,
    pub branch: Vec<&'a str>,
}

/// Every node, in reading order. `nodes` come in the provider's time
/// order, which orders siblings. `None` when `leaf` is not a node or its
/// chain to a root loops: the caller has no branch to show and reads
/// everything by time.
pub fn reading_order<'a>(nodes: &[TreeNode<'a>], leaf: &str) -> Option<Vec<Placed<'a>>> {
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id).collect();
    let parent_of = |n: &TreeNode<'a>| n.parent.filter(|p| ids.contains(p));
    let mut children: HashMap<Option<&str>, Vec<&'a str>> = HashMap::new();
    let mut parent: HashMap<&str, Option<&'a str>> = HashMap::new();
    for n in nodes {
        children.entry(parent_of(n)).or_default().push(n.id);
        parent.insert(n.id, parent_of(n));
    }

    let mut shown: Vec<&'a str> = Vec::new();
    let mut cursor = nodes.iter().find(|n| n.id == leaf).map(|n| n.id);
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return None;
        }
        shown.push(id);
        cursor = parent[id];
    }
    if shown.is_empty() {
        return None;
    }
    shown.reverse();

    let mut out = Vec::with_capacity(nodes.len());
    let mut placed = HashSet::new();
    walk(&shown, &[], &children, &parent, &mut placed, &mut out);
    Some(out)
}

/// Lay out one branch, `path` from its first message down: before each
/// message, the other versions of it; then the message.
fn walk<'a>(
    path: &[&'a str],
    branch: &[&'a str],
    children: &HashMap<Option<&str>, Vec<&'a str>>,
    parent: &HashMap<&str, Option<&'a str>>,
    placed: &mut HashSet<&'a str>,
    out: &mut Vec<Placed<'a>>,
) {
    for (i, &id) in path.iter().enumerate() {
        // The first message's siblings are its caller's to place.
        let first_of_a_branch = i == 0 && !branch.is_empty();
        if !first_of_a_branch {
            let siblings = children
                .get(&parent[id])
                .map(Vec::as_slice)
                .unwrap_or_default();
            for &other in siblings.iter().filter(|&&s| s != id) {
                if placed.contains(other) {
                    continue;
                }
                let mut inner = branch.to_vec();
                inner.push(other);
                let other_path = latest_path(other, children, placed);
                walk(&other_path, &inner, children, parent, placed, out);
            }
        }
        if placed.insert(id) {
            out.push(Placed {
                id,
                branch: branch.to_vec(),
            });
        }
    }
}

/// From `start` down through each message's latest child: the version a
/// branch ended on when it was left.
fn latest_path<'a>(
    start: &'a str,
    children: &HashMap<Option<&str>, Vec<&'a str>>,
    placed: &HashSet<&'a str>,
) -> Vec<&'a str> {
    let mut path = vec![start];
    let mut seen: HashSet<&str> = HashSet::from([start]);
    while let Some(&next) = children.get(&Some(*path.last().unwrap())).and_then(|c| {
        c.iter()
            .rev()
            .find(|c| !placed.contains(**c) && !seen.contains(**c))
    }) {
        seen.insert(next);
        path.push(next);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree<'a>(edges: &[(&'a str, Option<&'a str>)]) -> Vec<TreeNode<'a>> {
        edges
            .iter()
            .map(|&(id, parent)| TreeNode { id, parent })
            .collect()
    }

    fn order<'a>(nodes: &[TreeNode<'a>], leaf: &str) -> Vec<(&'a str, String)> {
        reading_order(nodes, leaf)
            .unwrap()
            .into_iter()
            .map(|p| (p.id, p.branch.join("/")))
            .collect()
    }

    /// The first prompt edited: the original exchange is kept, folded in
    /// before the edit, and the page reads on from the edit.
    #[test]
    fn an_edited_prompt_keeps_the_original_before_the_edit() {
        let nodes = tree(&[
            ("ask", None),
            ("answer", Some("ask")),
            ("follow-up", Some("answer")),
            ("reply", Some("follow-up")),
            ("ask-edited", None),
            ("answer-2", Some("ask-edited")),
        ]);
        assert_eq!(
            order(&nodes, "answer-2"),
            [
                ("ask", "ask".into()),
                ("answer", "ask".into()),
                ("follow-up", "ask".into()),
                ("reply", "ask".into()),
                ("ask-edited", String::new()),
                ("answer-2", String::new()),
            ]
        );
    }

    /// A regenerated answer, inside a branch that was itself left: the
    /// earlier answer nests in the branch it belongs to.
    #[test]
    fn a_regenerated_answer_in_a_left_branch_nests() {
        let nodes = tree(&[
            ("root", Some("hidden-root")),
            ("q1", Some("root")),
            ("a1", Some("q1")),
            ("a1-regenerated", Some("q1")),
            ("q1-edited", Some("root")),
            ("a1-edited", Some("q1-edited")),
        ]);
        assert_eq!(
            order(&nodes, "a1-edited"),
            [
                ("root", String::new()),
                ("q1", "q1".into()),
                ("a1", "q1/a1".into()),
                ("a1-regenerated", "q1".into()),
                ("q1-edited", String::new()),
                ("a1-edited", String::new()),
            ]
        );
    }

    #[test]
    fn a_tree_with_no_branches_reads_straight_through() {
        let nodes = tree(&[("a", None), ("b", Some("a")), ("c", Some("b"))]);
        assert_eq!(
            order(&nodes, "c"),
            [
                ("a", String::new()),
                ("b", String::new()),
                ("c", String::new())
            ]
        );
    }

    #[test]
    fn no_leaf_or_a_loop_has_no_order() {
        let nodes = tree(&[("a", Some("b")), ("b", Some("a"))]);
        assert!(reading_order(&nodes, "a").is_none());
        assert!(reading_order(&tree(&[("a", None)]), "missing").is_none());
    }
}
