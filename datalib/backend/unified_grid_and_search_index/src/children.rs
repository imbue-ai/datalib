//! A container's children, a page at a time around an anchor
//! (docs/dev/plans/documents_to_blocks.md, "A list starts at an anchor
//! and grows both ways"). Its children are the rows it holds, the
//! containers it holds that have no row, and whatever is filed under it.
//! The SQL here reads each of those three in `(sort_key, uuid)` order past
//! a key; [`window`] merges what they read into one page and its cursors.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// Where a child sorts among its siblings. Also a cursor: the edge row a
/// page stopped at, never a count of rows, so a child that arrives later
/// lands in its place without moving the pages either side of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub sort_key: String,
    pub uuid: String,
}

impl Key {
    pub fn new(sort_key: impl Into<String>, uuid: impl Into<String>) -> Self {
        Key {
            sort_key: sort_key.into(),
            uuid: uuid.into(),
        }
    }
}

/// How a child was found, best first: a uuid found more than one way is
/// listed once, as the best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Via {
    /// A `grid_rows` row whose parent is the container.
    Row,
    /// A `data_nodes` container whose parent is the container, with no
    /// row of its own.
    Node,
    /// Anything `filings` files under the container.
    Filed,
}

impl Via {
    pub const ALL: [Via; 3] = [Via::Row, Via::Node, Via::Filed];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub key: Key,
    pub via: Via,
}

/// Whose children a request lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parent {
    /// A container or a row, by uuid.
    Node(String),
    /// A configured source, by id: what it holds that names no parent.
    SourceTop(String),
}

impl Parent {
    /// `source:<id>` names a source's top; anything else is a uuid.
    pub fn parse(spelled: &str) -> Self {
        match spelled.strip_prefix("source:") {
            Some(id) => Parent::SourceTop(id.to_string()),
            None => Parent::Node(spelled.to_string()),
        }
    }

    /// What the reads bind as `?1`.
    pub fn bound(&self) -> &str {
        match self {
            Parent::Node(uuid) => uuid,
            Parent::SourceTop(id) => id,
        }
    }

    /// As a request spells it.
    pub fn spelled(&self) -> String {
        match self {
            Parent::Node(uuid) => uuid.clone(),
            Parent::SourceTop(id) => format!("source:{id}"),
        }
    }
}

/// Which children a request lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Only {
    /// Rows, containers and what is filed, as they come.
    All,
    /// The containers a person browses or filters by (`is_place`), each
    /// with whether more of them lie below it: a library's tree.
    Places,
}

/// Whose children a request lists, and which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub parent: Parent,
    pub only: Only,
}

impl Scope {
    pub fn all(parent: &str) -> Self {
        Scope {
            parent: Parent::parse(parent),
            only: Only::All,
        }
    }

    pub fn places(parent: &str) -> Self {
        Scope {
            parent: Parent::parse(parent),
            only: Only::Places,
        }
    }

    /// The ways this scope finds children, best first. A source's top has
    /// nothing filed under it, and a place is always a container.
    pub fn vias(&self) -> Vec<Via> {
        Via::ALL
            .into_iter()
            .filter(|via| from(self, *via).is_some())
            .collect()
    }
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Up to `limit` children before the anchor and `limit` after it, an
    /// item anchor itself between them; a moment's first child counts as
    /// after it.
    Around(Anchor),
    /// The `limit` children after this one.
    After(Key),
    /// The `limit` children before this one.
    Before(Key),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// The first child.
    Start,
    /// A child, by uuid.
    Item(String),
    /// The first child whose sort key is at or after this moment, written
    /// as a sort key is (`created_at_utc`'s form).
    Moment(String),
}

/// Where a request divides the children, and how many it wants each side.
/// The children before `at` are read nearest first; those from it on in
/// order, `at` itself among them when `at_is_after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub at: Key,
    pub at_is_after: bool,
    pub before: usize,
    pub after: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildrenError {
    NotAChild { parent: String, child: String },
}

impl std::fmt::Display for ChildrenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChildrenError::NotAChild { parent, child } => {
                write!(f, "{child} is not a child of {parent}")
            }
        }
    }
}

/// Why a page of children could not be read.
#[derive(Debug)]
pub enum ChildrenFailure {
    Refused(ChildrenError),
    Repo(datalib_core::repo::RepoError),
}

impl From<ChildrenError> for ChildrenFailure {
    fn from(e: ChildrenError) -> Self {
        ChildrenFailure::Refused(e)
    }
}

impl From<datalib_core::repo::RepoError> for ChildrenFailure {
    fn from(e: datalib_core::repo::RepoError) -> Self {
        ChildrenFailure::Repo(e)
    }
}

/// `item_key` is the anchor item's sort key among `parent`'s children,
/// where it is one.
pub fn split(
    parent: &str,
    ask: &Ask,
    item_key: Option<String>,
    limit: usize,
) -> Result<Split, ChildrenError> {
    let around = |at: Key| Split {
        at,
        at_is_after: true,
        before: limit,
        after: limit,
    };
    Ok(match ask {
        Ask::Around(Anchor::Start) => around(Key::new("", "")),
        Ask::Around(Anchor::Moment(at)) => around(Key::new(at.as_str(), "")),
        Ask::Around(Anchor::Item(uuid)) => {
            let sort_key = item_key.ok_or_else(|| ChildrenError::NotAChild {
                parent: parent.to_string(),
                child: uuid.clone(),
            })?;
            Split {
                after: limit + 1,
                ..around(Key::new(sort_key, uuid.as_str()))
            }
        }
        Ask::After(key) => Split {
            at: key.clone(),
            at_is_after: false,
            before: 0,
            after: limit,
        },
        Ask::Before(key) => Split {
            at: key.clone(),
            at_is_after: true,
            before: limit,
            after: 0,
        },
    })
}

/// One page of children in order, and the cursor past each end of it:
/// `None` where nothing lies that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    pub keys: Vec<Key>,
    pub before: Option<Key>,
    pub after: Option<Key>,
}

/// The page `split` asks for, from what each way of finding children read
/// on each side of it: `before[i]` nearest first and `after[i]` in order,
/// each read with a limit one more than that side wants, so a list that
/// comes back that long says there may be more past it.
pub fn window(split: &Split, before: Vec<Vec<Found>>, after: Vec<Vec<Found>>) -> Cut {
    let (older, more_before) = side(before, split.before, |a, b| b.cmp(a));
    let (newer, more_after) = side(after, split.after, Ord::cmp);
    let mut page: Vec<Found> = older.into_iter().rev().chain(newer).collect();
    dedupe(&mut page);
    let edge = |row: Option<&Found>| row.map_or_else(|| split.at.clone(), |f| f.key.clone());
    Cut {
        before: more_before.then(|| edge(page.first())),
        after: more_after.then(|| edge(page.last())),
        keys: page.into_iter().map(|f| f.key).collect(),
    }
}

/// One side's children in `order`, at most `want` of them, and whether
/// there are more past them. A child found two ways has one key (a filing
/// takes its row's or its container's), so the first `want` of the merged
/// lists are the first `want` children. A container its documents carry
/// at different keys is listed once, at its first; the list it came in
/// then reads short, so a full list says there may be more.
fn side(
    lists: Vec<Vec<Found>>,
    want: usize,
    order: impl Fn(&Key, &Key) -> std::cmp::Ordering,
) -> (Vec<Found>, bool) {
    let any_full = lists.iter().any(|l| l.len() > want);
    let mut rows: Vec<Found> = lists.into_iter().flatten().collect();
    rows.sort_by(|a, b| order(&a.key, &b.key).then(a.via.cmp(&b.via)));
    dedupe(&mut rows);
    let more = rows.len() > want || any_full;
    rows.truncate(want);
    (rows, more)
}

/// Each uuid once, where it was found the best way.
fn dedupe(rows: &mut Vec<Found>) {
    let mut best: HashMap<String, Via> = HashMap::new();
    for f in rows.iter() {
        best.entry(f.key.uuid.clone())
            .and_modify(|v| *v = (*v).min(f.via))
            .or_insert(f.via);
    }
    let mut kept: HashSet<String> = HashSet::new();
    rows.retain(|f| best[&f.key.uuid] == f.via && kept.insert(f.key.uuid.clone()));
}

/// What a child's row says of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowFacts {
    pub kind: String,
    pub author: Option<String>,
    pub author_handle: Option<String>,
    pub created_at: Option<String>,
    pub preview: Option<String>,
    pub markdown_uuid: Option<String>,
}

/// What a child's container says of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeFacts {
    pub kind: String,
    pub name: Option<String>,
}

/// One child as a page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Child {
    pub uuid: String,
    pub sort_key: String,
    /// It is a container, with children of its own.
    pub container: bool,
    /// Its container's kind where it is one, else its row's.
    pub kind: String,
    pub name: Option<String>,
    pub author: Option<String>,
    /// The author as a handle (`email:…`), which a person chip names.
    pub author_handle: Option<String>,
    pub created_at: Option<String>,
    pub preview: Option<String>,
    /// The document its row is in; `None` for a container with no row.
    pub markdown_uuid: Option<String>,
    /// Listing places: whether places lie below it. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub places_below: Option<bool>,
}

/// `places_below` names, listing places, every child with places below
/// it; `None` when the listing is not of places.
pub fn describe(
    keys: Vec<Key>,
    rows: &HashMap<String, RowFacts>,
    nodes: &HashMap<String, NodeFacts>,
    places_below: Option<&HashSet<String>>,
) -> Vec<Child> {
    keys.into_iter()
        .map(|key| {
            let row = rows.get(&key.uuid).cloned().unwrap_or_default();
            let node = nodes.get(&key.uuid);
            Child {
                container: node.is_some(),
                kind: node.map_or(row.kind, |n| n.kind.clone()),
                name: node.and_then(|n| n.name.clone()),
                author: row.author,
                author_handle: row.author_handle,
                created_at: row.created_at,
                preview: row.preview,
                markdown_uuid: row.markdown_uuid,
                places_below: places_below.map(|below| below.contains(&key.uuid)),
                sort_key: key.sort_key,
                uuid: key.uuid,
            }
        })
        .collect()
}

/// A page of a container's children and the commit it was read at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildrenPage {
    pub children: Vec<Child>,
    pub before: Option<Key>,
    pub after: Option<Key>,
    pub at: Option<String>,
}

/// Which side of a split one read is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

/// The children found `via` on `side` of a split, as `(sort_key, uuid)`
/// rows in that side's order. Binds the parent as `?1`, the split's key as
/// `?2, ?3` and the limit as `?4`. Rows and containers are read off the
/// `(parent, sort_key, uuid)` indexes in order; what is filed takes its key
/// from its row or container, so it is sorted after it is found.
/// The read of `scope`'s children found `via`, past `?2`/`?3` on `side`,
/// `?4` of them; `None` where the scope finds none that way.
pub fn children_sql(scope: &Scope, via: Via, side: Side, at_is_after: bool) -> Option<String> {
    let (op, dir) = match (side, at_is_after) {
        (Side::Before, true) => ("<", "DESC"),
        (Side::Before, false) => ("<=", "DESC"),
        (Side::After, true) => (">=", "ASC"),
        (Side::After, false) => (">", "ASC"),
    };
    let from = from(scope, via)?;
    // A container is carried by every document that names it, and so is
    // a filing; a row is one row.
    let distinct = if via == Via::Row { "" } else { "DISTINCT " };
    Some(format!(
        "SELECT {distinct}sort_key, uuid FROM {from} AND (sort_key, uuid) {op} (?2, ?3) \
         ORDER BY sort_key {dir}, uuid {dir} LIMIT ?4"
    ))
}

/// Where `scope`'s children found `via` are, up to a `WHERE` that more
/// conditions can follow, `?1` its parent.
fn from(scope: &Scope, via: Via) -> Option<&'static str> {
    let top = matches!(scope.parent, Parent::SourceTop(_));
    Some(match (via, scope.only, top) {
        (Via::Row, Only::All, false) => "grid_rows WHERE parent_uuid = ?1",
        (Via::Row, Only::All, true) => "grid_rows WHERE parent_uuid IS NULL AND source_id = ?1",
        (Via::Node, Only::All, false) => {
            "data_nodes n WHERE parent_uuid = ?1 \
             AND NOT EXISTS (SELECT 1 FROM grid_rows g WHERE g.uuid = n.uuid)"
        }
        (Via::Node, Only::All, true) => {
            "data_nodes n WHERE parent_uuid IS NULL AND source_id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM grid_rows g WHERE g.uuid = n.uuid)"
        }
        (Via::Node, Only::Places, false) => "data_nodes n WHERE parent_uuid = ?1 AND is_place = 1",
        (Via::Node, Only::Places, true) => {
            "data_nodes n WHERE parent_uuid IS NULL AND source_id = ?1 AND is_place = 1"
        }
        (Via::Filed, Only::All, false) => FILED,
        (Via::Filed, Only::Places, false) => FILED_PLACES,
        (Via::Row, Only::Places, _) | (Via::Filed, _, true) => return None,
    })
}

/// What is filed under `?1`, each with the sort key of its row, or else
/// of its container.
const FILED: &str = "(SELECT f.node_uuid AS uuid, \
     COALESCE((SELECT g.sort_key FROM grid_rows g WHERE g.uuid = f.node_uuid), \
              (SELECT MIN(n.sort_key) FROM data_nodes n WHERE n.uuid = f.node_uuid), '') \
       AS sort_key \
     FROM filings f WHERE f.under_uuid = ?1) WHERE 1";

/// The places filed under `?1`, each with its container's sort key.
const FILED_PLACES: &str = "(SELECT f.node_uuid AS uuid, \
     (SELECT MIN(n.sort_key) FROM data_nodes n WHERE n.uuid = f.node_uuid) AS sort_key \
     FROM filings f WHERE f.under_uuid = ?1 \
       AND EXISTS (SELECT 1 FROM data_nodes n WHERE n.uuid = f.node_uuid AND n.is_place = 1)) \
     WHERE 1";

/// The sort key of `?2` among `scope`'s children, `?1` its parent, found
/// the best way it is one; no row when it is not one.
pub fn child_key_sql(scope: &Scope) -> String {
    let each: Vec<String> = scope
        .vias()
        .into_iter()
        .filter_map(|via| {
            let from = from(scope, via)?;
            Some(format!(
                "SELECT * FROM (SELECT {} AS via, sort_key FROM {from} AND uuid = ?2 \
                 ORDER BY sort_key LIMIT 1)",
                via as u8
            ))
        })
        .collect();
    format!(
        "SELECT sort_key FROM ({}) ORDER BY via LIMIT 1",
        each.join(" UNION ALL ")
    )
}

/// Which of the places in the JSON array `?1` have places below them,
/// held or filed.
pub const PLACES_BELOW_SQL: &str = "SELECT DISTINCT parent_uuid FROM data_nodes \
     WHERE is_place = 1 AND parent_uuid IN (SELECT value FROM json_each(?1)) \
     UNION SELECT DISTINCT f.under_uuid FROM filings f \
     WHERE f.under_uuid IN (SELECT value FROM json_each(?1)) \
       AND EXISTS (SELECT 1 FROM data_nodes n WHERE n.uuid = f.node_uuid AND n.is_place = 1)";

/// What a page's rows say of them, for the uuids in the JSON array `?1`.
pub const ROW_FACTS_SQL: &str =
    "SELECT uuid, kind, author, author_handle, created_at, preview, markdown_uuid \
     FROM grid_rows WHERE uuid IN (SELECT value FROM json_each(?1))";

/// What a page's containers say of them, for the uuids in the JSON array
/// `?1`: one container is carried by every document that names it.
pub const NODE_FACTS_SQL: &str = "SELECT uuid, MAX(kind), MAX(name) \
     FROM data_nodes WHERE uuid IN (SELECT value FROM json_each(?1)) GROUP BY uuid";

#[cfg(test)]
mod tests {
    use super::*;

    fn found(sort_key: &str, uuid: &str, via: Via) -> Found {
        Found {
            key: Key::new(sort_key, uuid),
            via,
        }
    }

    fn uuids(cut: &Cut) -> Vec<&str> {
        cut.keys.iter().map(|k| k.uuid.as_str()).collect()
    }

    fn after(limit: usize) -> Split {
        split("p", &Ask::Around(Anchor::Start), None, limit).unwrap()
    }

    /// Rows, containers and filed children make one list in
    /// `(sort_key, uuid)` order, a tie on the key broken by uuid.
    #[test]
    fn the_three_ways_merge_in_key_order_ties_by_uuid() {
        let cut = window(
            &after(10),
            vec![vec![], vec![], vec![]],
            vec![
                vec![found("1", "a", Via::Row), found("3", "c", Via::Row)],
                vec![found("2", "b", Via::Node), found("3", "a2", Via::Node)],
                vec![found("0", "z", Via::Filed)],
            ],
        );
        assert_eq!(uuids(&cut), ["z", "a", "b", "a2", "c"]);
        assert_eq!((cut.before, cut.after), (None, None));
    }

    /// A uuid found as a row and as filed is listed once, as the row.
    #[test]
    fn a_child_found_two_ways_is_listed_once_as_the_better() {
        let cut = window(
            &after(10),
            vec![vec![], vec![], vec![]],
            vec![
                vec![found("5", "t", Via::Row)],
                vec![],
                vec![found("1", "t", Via::Filed), found("2", "u", Via::Filed)],
            ],
        );
        assert_eq!(uuids(&cut), ["u", "t"]);
        assert_eq!(cut.keys[1].sort_key, "5");
    }

    /// A page is the first `limit` of the merged lists; what lies past
    /// it waits for the next page, and the cursor is the page's last row.
    #[test]
    fn a_page_stops_at_its_limit_and_points_past_its_last_row() {
        let cut = window(
            &after(2),
            vec![vec![], vec![], vec![]],
            vec![
                vec![
                    found("1", "a", Via::Row),
                    found("2", "b", Via::Row),
                    found("3", "c", Via::Row),
                ],
                vec![found("0", "early", Via::Node)],
                vec![],
            ],
        );
        assert_eq!(uuids(&cut), ["early", "a"]);
        assert_eq!(cut.after, Some(Key::new("1", "a")));
    }

    /// A container its documents carry at two keys is listed once, at the
    /// first; its list came back full, so the page says more may follow
    /// though it holds fewer than asked.
    #[test]
    fn a_container_carried_at_two_keys_is_listed_once_and_more_may_follow() {
        let cut = window(
            &after(1),
            vec![vec![], vec![], vec![]],
            vec![
                vec![],
                vec![found("1", "chan", Via::Node), found("2", "chan", Via::Node)],
                vec![],
            ],
        );
        assert_eq!(uuids(&cut), ["chan"]);
        assert_eq!(cut.after, Some(Key::new("1", "chan")));
        let cut = window(
            &after(2),
            vec![vec![], vec![], vec![]],
            vec![
                vec![],
                vec![found("1", "chan", Via::Node), found("2", "chan", Via::Node)],
                vec![],
            ],
        );
        assert_eq!(cut.after, None);
    }

    /// Around an item: the side before it nearest first, read back into
    /// order, then the item and as many again after it; each cursor is
    /// its edge row.
    #[test]
    fn around_an_item_both_sides_and_their_cursors() {
        let s = split(
            "p",
            &Ask::Around(Anchor::Item("m3".into())),
            Some("3".into()),
            2,
        )
        .unwrap();
        assert_eq!(s.at, Key::new("3", "m3"));
        assert_eq!((s.before, s.after), (2, 3), "the item and two after it");
        let cut = window(
            &s,
            vec![
                vec![
                    found("2", "m2", Via::Row),
                    found("1", "m1", Via::Row),
                    found("0", "m0", Via::Row),
                ],
                vec![],
                vec![],
            ],
            vec![
                vec![found("3", "m3", Via::Row), found("4", "m4", Via::Row)],
                vec![],
                vec![],
            ],
        );
        assert_eq!(uuids(&cut), ["m1", "m2", "m3", "m4"]);
        assert_eq!(cut.before, Some(Key::new("1", "m1")));
        assert_eq!(cut.after, None);
    }

    /// Paging on from a cursor: the side it came from is only probed, and
    /// says whether anything lies that way.
    #[test]
    fn a_page_past_a_cursor_points_back_at_itself() {
        let s = split("p", &Ask::After(Key::new("4", "m4")), None, 2).unwrap();
        let cut = window(
            &s,
            vec![vec![found("4", "m4", Via::Row)], vec![], vec![]],
            vec![vec![found("5", "m5", Via::Row)], vec![], vec![]],
        );
        assert_eq!(uuids(&cut), ["m5"]);
        assert_eq!(cut.before, Some(Key::new("5", "m5")));
        assert_eq!(cut.after, None);
    }

    /// Nothing either side: no rows, and no cursor to page on with.
    #[test]
    fn empty_sides_have_no_cursors() {
        let s = split("p", &Ask::Around(Anchor::Moment("2026".into())), None, 5).unwrap();
        let cut = window(&s, vec![vec![]; 3], vec![vec![]; 3]);
        assert_eq!(
            cut,
            Cut {
                keys: vec![],
                before: None,
                after: None
            }
        );
        let s = split("p", &Ask::Before(Key::new("1", "a")), None, 5).unwrap();
        let cut = window(
            &s,
            vec![vec![]; 3],
            vec![vec![found("1", "a", Via::Row)], vec![], vec![]],
        );
        assert_eq!(uuids(&cut), Vec::<&str>::new());
        assert_eq!((cut.before, cut.after), (None, Some(Key::new("1", "a"))));
    }

    /// An anchor that is not among the container's children is refused by
    /// name, never read as the start.
    #[test]
    fn an_anchor_that_is_not_a_child_is_refused() {
        let refused = split(
            "chan",
            &Ask::Around(Anchor::Item("elsewhere".into())),
            None,
            5,
        );
        assert_eq!(
            refused,
            Err(ChildrenError::NotAChild {
                parent: "chan".into(),
                child: "elsewhere".into()
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            "elsewhere is not a child of chan"
        );
    }

    /// A container's own kind and name win over its row's kind; a row
    /// that is no container keeps its own.
    #[test]
    fn a_container_is_described_by_its_node_and_its_row() {
        let rows = HashMap::from([
            (
                "t".to_string(),
                RowFacts {
                    kind: "Message".into(),
                    author: Some("Data".into()),
                    author_handle: Some("slack:T1/U_DATA".into()),
                    ..Default::default()
                },
            ),
            (
                "m".to_string(),
                RowFacts {
                    kind: "Message".into(),
                    ..Default::default()
                },
            ),
        ]);
        let nodes = HashMap::from([(
            "t".to_string(),
            NodeFacts {
                kind: "Slack thread".into(),
                name: Some("Warp core".into()),
            },
        )]);
        let out = describe(
            vec![Key::new("1", "t"), Key::new("2", "m")],
            &rows,
            &nodes,
            None,
        );
        assert!(out[0].container && out[0].kind == "Slack thread");
        assert_eq!(out[0].author.as_deref(), Some("Data"));
        assert_eq!(out[0].author_handle.as_deref(), Some("slack:T1/U_DATA"));
        assert!(!out[1].container && out[1].kind == "Message");
    }
}
