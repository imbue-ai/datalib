//! Which pages a roots-mode run mirrors: those whose chain of parents in
//! the outline reaches a root. Decided here over values; the run reads
//! the outline and fetches what this names.

use std::collections::{HashMap, HashSet};

use super::schema_raw::OutlineKind;

/// One object's place in the outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub kind: OutlineKind,
    pub parent: Option<String>,
}

/// The pages of `outline` under one of `roots`, a root page included: a
/// child page at any depth, a database's rows and what hangs off them,
/// a page inside a block of a page. A chain that leaves the outline, or
/// loops, ends without reaching a root.
pub fn pages_under(outline: &HashMap<String, Placed>, roots: &HashSet<String>) -> HashSet<String> {
    let mut known: HashMap<&str, bool> = HashMap::new();
    outline
        .iter()
        .filter(|(_, placed)| placed.kind == OutlineKind::Page)
        .filter(|(id, _)| reaches_a_root(id, outline, roots, &mut known))
        .map(|(id, _)| id.clone())
        .collect()
}

fn reaches_a_root<'a>(
    start: &'a str,
    outline: &'a HashMap<String, Placed>,
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
        at = outline.get(id).and_then(|p| p.parent.as_deref());
    };
    for id in chain {
        known.insert(id, answer);
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(kind: OutlineKind, parent: Option<&str>) -> Placed {
        Placed {
            kind,
            parent: parent.map(String::from),
        }
    }

    fn outline(rows: &[(&str, OutlineKind, Option<&str>)]) -> HashMap<String, Placed> {
        rows.iter()
            .map(|(id, kind, parent)| (id.to_string(), placed(*kind, *parent)))
            .collect()
    }

    fn under(o: &HashMap<String, Placed>, roots: &[&str]) -> Vec<String> {
        let roots: HashSet<String> = roots.iter().map(|r| r.to_string()).collect();
        let mut out: Vec<String> = pages_under(o, &roots).into_iter().collect();
        out.sort();
        out
    }

    use OutlineKind::{Block, Database, Page};

    /// The case that sent this in: a database pasted as a root mirrored
    /// nothing, though its rows and their sub-pages are what was wanted.
    #[test]
    fn a_database_root_brings_its_rows_and_what_hangs_off_them() {
        let o = outline(&[
            ("missions", Database, Some("workspace")),
            ("away-team", Page, Some("missions")),
            ("debrief", Page, Some("away-team")),
            ("crew", Database, Some("workspace")),
            ("picard", Page, Some("crew")),
        ]);
        assert_eq!(under(&o, &["missions"]), ["away-team", "debrief"]);
    }

    /// A child page is found by its parent, whatever the root's body
    /// says, at any depth, and through a block (a toggle, a column) and a
    /// database inside a page.
    #[test]
    fn a_page_root_brings_every_page_under_it() {
        let o = outline(&[
            ("bridge", Page, Some("workspace")),
            ("ops", Page, Some("bridge")),
            ("toggle", Block, Some("bridge")),
            ("column", Block, Some("toggle")),
            ("helm", Page, Some("column")),
            ("logs", Database, Some("ops")),
            ("stardate-1", Page, Some("logs")),
            ("sickbay", Page, Some("workspace")),
        ]);
        assert_eq!(
            under(&o, &["bridge"]),
            ["bridge", "helm", "ops", "stardate-1"]
        );
    }

    /// A parent the credential cannot see ends the chain, and so does a
    /// loop, rather than spinning.
    #[test]
    fn a_chain_that_leaves_the_outline_or_loops_reaches_nothing() {
        let o = outline(&[
            ("orphan", Page, Some("unseen")),
            ("a", Page, Some("b")),
            ("b", Page, Some("a")),
            ("loose-block", Block, None),
            ("in-loose", Page, Some("loose-block")),
        ]);
        assert!(under(&o, &["bridge"]).is_empty());
        assert_eq!(under(&o, &["a"]), ["a", "b"]);
    }

    /// One answer per chain is shared, so siblings deep under one root
    /// and pages outside it are told apart in one pass.
    #[test]
    fn siblings_share_the_answer_for_their_chain() {
        let o = outline(&[
            ("root", Page, Some("workspace")),
            ("mid", Page, Some("root")),
            ("leaf-1", Page, Some("mid")),
            ("leaf-2", Page, Some("mid")),
            ("other", Page, Some("workspace")),
            ("other-leaf", Page, Some("other")),
        ]);
        assert_eq!(under(&o, &["mid"]), ["leaf-1", "leaf-2", "mid"]);
    }
}
