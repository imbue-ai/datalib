//! Resolve human-facing mailbox **label paths** to JMAP mailbox ids.
//!
//! Mailboxes (Fastmail "folders"/labels, Gmail labels) are a tree: each
//! has a display `name` and an optional `parentId`. A mailbox's full
//! *path* is the chain of `name`s from the root joined with `/`
//! (`Work`, `Work/Projects`, `Work/Projects/Q3`). Fastmail forbids `/`
//! inside a single label name, so a `/` in a configured path is
//! unambiguously a parent/child separator.
//!
//! This is the single matcher shared by the extract-time and
//! render-time label filters, and it is source-agnostic: a JMAP account
//! exposes the tree via `Mailbox/get` (`parentId`), while a Google
//! Takeout `.mbox` stores each Gmail label as a flat mailbox whose
//! `name` is already the full `Parent/Child` string with no parent — so
//! the same walk yields the same path for both, and the same configured
//! labels mean the same thing regardless of which backend produced the
//! raw store.
//!
//! Matching is **exact** on the full path: a configured `Work` matches
//! only the mailbox at `Work`, never `Work/Projects`. List nested
//! mailboxes explicitly to include them.

use std::collections::{HashMap, HashSet};

/// Max parent-chain depth we'll walk before bailing — a guard against a
/// malformed or cyclic `parentId` graph wedging the resolver.
const MAX_DEPTH: usize = 64;

/// One mailbox, reduced to what path resolution needs: its id, display
/// `name`, and optional `parent_id`.
#[derive(Debug, Clone)]
pub struct MailboxNode {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
}

impl MailboxNode {
    /// Pull `(id, name, parentId)` out of a stored JMAP mailbox payload
    /// (the shape returned by `RawDb::load_mailboxes` / held in
    /// `ParsedEmail::mailboxes`). Returns `None` when there's no `id`.
    pub fn from_payload(v: &serde_json::Value) -> Option<Self> {
        let id = v.get("id")?.as_str()?.to_string();
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let parent_id = v
            .get("parentId")
            .and_then(|p| p.as_str())
            .map(str::to_string);
        Some(Self {
            id,
            name,
            parent_id,
        })
    }
}

/// Build the full `Parent/Child` path for every mailbox, keyed by id.
pub fn paths_by_id(nodes: &[MailboxNode]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &MailboxNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out = HashMap::with_capacity(nodes.len());
    for n in nodes {
        let mut segs: Vec<&str> = Vec::new();
        let mut cur = Some(n);
        let mut depth = 0;
        while let Some(node) = cur {
            segs.push(node.name.as_str());
            depth += 1;
            if depth >= MAX_DEPTH {
                break;
            }
            cur = node
                .parent_id
                .as_deref()
                .and_then(|pid| by_id.get(pid).copied());
        }
        segs.reverse();
        out.insert(n.id.clone(), segs.join("/"));
    }
    out
}

/// Outcome of resolving a label-path list against a mailbox set.
#[derive(Debug, Default)]
pub struct Resolved {
    /// Mailbox ids whose full path exactly equals one of the requested
    /// labels.
    pub ids: HashSet<String>,
    /// Requested labels that matched no mailbox — almost always a typo
    /// or a label that doesn't exist in this account. Callers log these
    /// so a misspelled filter doesn't silently drop every message.
    pub unmatched: Vec<String>,
}

/// Resolve `labels` (exact full-path match) against `nodes` to the set
/// of matching mailbox ids, plus any labels that matched nothing.
///
/// Leading/trailing whitespace on each requested label is trimmed (so
/// YAML list entries needn't be fussy); the stored mailbox paths are
/// matched verbatim and case-sensitively.
pub fn resolve(nodes: &[MailboxNode], labels: &[String]) -> Resolved {
    let paths = paths_by_id(nodes);
    // path -> ids. A path *should* be unique, but two sibling mailboxes
    // with the same name (or a re-used Gmail label) could collide;
    // matching all of them is the conservative choice.
    let mut by_path: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, path) in &paths {
        by_path.entry(path.as_str()).or_default().push(id.as_str());
    }
    let mut resolved = Resolved::default();
    for label in labels {
        let want = label.trim();
        match by_path.get(want) {
            Some(matched) => resolved.ids.extend(matched.iter().map(|s| s.to_string())),
            None => resolved.unmatched.push(label.clone()),
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, name: &str, parent: Option<&str>) -> MailboxNode {
        MailboxNode {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: parent.map(str::to_string),
        }
    }

    #[test]
    fn builds_nested_paths_from_parent_chain() {
        let nodes = vec![
            node("a", "Work", None),
            node("b", "Projects", Some("a")),
            node("c", "Q3", Some("b")),
            node("d", "Inbox", None),
        ];
        let paths = paths_by_id(&nodes);
        assert_eq!(paths["a"], "Work");
        assert_eq!(paths["b"], "Work/Projects");
        assert_eq!(paths["c"], "Work/Projects/Q3");
        assert_eq!(paths["d"], "Inbox");
    }

    #[test]
    fn flat_mbox_label_is_its_own_path() {
        // Gmail-takeout style: the full label is the `name`, no parent.
        let nodes = vec![node("x", "Work/Projects", None)];
        let paths = paths_by_id(&nodes);
        assert_eq!(paths["x"], "Work/Projects");
    }

    #[test]
    fn exact_match_only_no_subtree() {
        let nodes = vec![node("a", "Work", None), node("b", "Projects", Some("a"))];
        let r = resolve(&nodes, &["Work".to_string()]);
        assert_eq!(r.ids, HashSet::from(["a".to_string()]));
        assert!(r.unmatched.is_empty());

        let r = resolve(&nodes, &["Work/Projects".to_string()]);
        assert_eq!(r.ids, HashSet::from(["b".to_string()]));
    }

    #[test]
    fn reports_unmatched_labels() {
        let nodes = vec![node("a", "Inbox", None)];
        let r = resolve(&nodes, &["Inbox".to_string(), "Typo".to_string()]);
        assert_eq!(r.ids, HashSet::from(["a".to_string()]));
        assert_eq!(r.unmatched, vec!["Typo".to_string()]);
    }

    #[test]
    fn from_payload_reads_jmap_shape() {
        let n = MailboxNode::from_payload(&json!({
            "id": "m1", "name": "Projects", "parentId": "m0"
        }))
        .unwrap();
        assert_eq!(n.id, "m1");
        assert_eq!(n.name, "Projects");
        assert_eq!(n.parent_id.as_deref(), Some("m0"));

        let root = MailboxNode::from_payload(&json!({"id": "m0", "name": "Work"})).unwrap();
        assert_eq!(root.parent_id, None);
    }
}
