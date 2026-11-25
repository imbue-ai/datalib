//! Golden of where every TNG row sits: each source's tree of containers
//! and rows, in `(sort_key, uuid)` order, with what each
//! container is (read together, a place) and what else each row is filed
//! under. Before the snapshot, the rules `docs/dev/grid_rows.md` §"Where a
//! row sits" states: every parent and filing is a row, no row is its own
//! ancestor, `read_uuid` and `place_uuid` are the nearest container above
//! of each kind, a place lives in a place, and what is filed is read
//! together and no place. Over `:ingested_tng` and over each source synced
//! on its own.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::fixture_db_snapshot::fixture_db_path;
use crate::fixture_terms_snapshot::lightroom_index_path;

#[derive(sqlx::FromRow)]
struct Placed {
    uuid: String,
    source_id: String,
    kind: String,
    name: Option<String>,
    conversation_uuid: String,
    is_document: bool,
    parent_uuid: Option<String>,
    read_uuid: Option<String>,
    place_uuid: Option<String>,
    diff_status: Option<String>,
}

/// A container's roles.
#[derive(Clone, Copy, Default)]
struct Roles {
    read_together: bool,
    is_place: bool,
}

struct Tree {
    rows: HashMap<String, Placed>,
    roles: HashMap<String, Roles>,
    /// Each parent's children, in `(sort_key, uuid)` order.
    children: HashMap<String, Vec<String>>,
    /// What each row is filed under besides its parent.
    filed_under: BTreeMap<String, Vec<String>>,
}

impl Tree {
    async fn read(path: &Path) -> Tree {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let placed: Vec<Placed> = sqlx::query_as(
            "SELECT uuid, source_id, kind, conversation_name AS name, conversation_uuid, is_document, \
                    parent_uuid, read_uuid, place_uuid, diff_status \
               FROM grid_rows ORDER BY parent_uuid, sort_key, uuid",
        )
        .fetch_all(&pool)
        .await
        .expect("read where the rows sit");
        let roles: Vec<(String, bool, bool)> =
            sqlx::query_as("SELECT uuid, read_together, is_place FROM nodes")
                .fetch_all(&pool)
                .await
                .expect("read the containers");
        let filings: Vec<(String, String)> = sqlx::query_as(
            "SELECT node_uuid, under_uuid FROM filings ORDER BY node_uuid, under_uuid",
        )
        .fetch_all(&pool)
        .await
        .expect("read the filings");
        pool.close().await;

        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for row in &placed {
            if let Some(parent) = &row.parent_uuid {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(row.uuid.clone());
            }
        }
        let mut filed_under: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (node, under) in filings {
            filed_under.entry(node).or_default().push(under);
        }
        Tree {
            rows: placed.into_iter().map(|r| (r.uuid.clone(), r)).collect(),
            roles: roles
                .into_iter()
                .map(|(uuid, read_together, is_place)| {
                    (
                        uuid,
                        Roles {
                            read_together,
                            is_place,
                        },
                    )
                })
                .collect(),
            children,
            filed_under,
        }
    }

    /// Every way a row breaks the rules, one line each. A diff group
    /// carries no containers (`render_diff.rs`), so its rows name parents
    /// the index has no row for; they are drawn as such in the snapshot
    /// and left out of the rules.
    fn broken(&self) -> Vec<String> {
        let mut out = Vec::new();
        for row in self.rows.values() {
            if row.diff_status.is_some() {
                continue;
            }
            let mut read = None;
            let mut place = None;
            let mut seen = HashSet::from([row.uuid.as_str()]);
            let mut above = row.parent_uuid.as_deref();
            while let Some(uuid) = above {
                if !seen.insert(uuid) {
                    out.push(format!("{}: is its own ancestor", row.uuid));
                    break;
                }
                let Some(parent) = self.rows.get(uuid) else {
                    out.push(format!("{}: its ancestor {uuid} is not a row", row.uuid));
                    break;
                };
                let roles = self.roles.get(uuid).copied().unwrap_or_default();
                if read.is_none() && roles.read_together {
                    read = Some(uuid);
                }
                if place.is_none() && roles.is_place {
                    place = Some(uuid);
                }
                above = parent.parent_uuid.as_deref();
            }
            let is_place = self.roles.get(&row.uuid).is_some_and(|r| r.is_place);
            if is_place && row.parent_uuid.as_deref() != place {
                out.push(format!(
                    "{}: a place in {:?}, which is no place",
                    row.uuid, row.parent_uuid
                ));
            }
            if row.read_uuid.as_deref() != read {
                out.push(format!(
                    "{}: read_uuid {:?}, but the nearest container read together above is {read:?}",
                    row.uuid, row.read_uuid
                ));
            }
            if row.place_uuid.as_deref() != place {
                out.push(format!(
                    "{}: place_uuid {:?}, but the nearest place above is {place:?}",
                    row.uuid, row.place_uuid
                ));
            }
        }
        for (node, unders) in &self.filed_under {
            let read_there = self
                .rows
                .get(node)
                .is_some_and(|row| row.conversation_uuid == *node);
            let is_place = self.roles.get(node).is_some_and(|r| r.is_place);
            if !read_there || is_place {
                out.push(format!(
                    "filing {node} under {unders:?}: what is filed is a document or a \
                     container read together, and no place"
                ));
            }
            for uuid in std::iter::once(node).chain(unders) {
                if !self.rows.contains_key(uuid) {
                    out.push(format!(
                        "filing {node} under {unders:?}: {uuid} is not a row"
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// One tree per source: its rows with no parent, or whose parent is
    /// another source's or no row at all, and everything under them.
    fn render(&self) -> BTreeMap<String, String> {
        let mut tops: Vec<&Placed> = self
            .rows
            .values()
            .filter(|row| {
                row.parent_uuid
                    .as_ref()
                    .and_then(|p| self.rows.get(p))
                    .is_none_or(|parent| parent.source_id != row.source_id)
            })
            .collect();
        tops.sort_by(|a, b| (&a.parent_uuid, &a.uuid).cmp(&(&b.parent_uuid, &b.uuid)));
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for top in tops {
            let text = out.entry(top.source_id.clone()).or_default();
            if let Some(parent) = &top.parent_uuid {
                match self.rows.get(parent) {
                    Some(p) => writeln!(text, "under {parent} ({})", p.source_id).unwrap(),
                    None => writeln!(text, "under {parent} (not a row)").unwrap(),
                }
            }
            self.draw(top, 0, text);
        }
        out
    }

    fn draw(&self, row: &Placed, depth: usize, out: &mut String) {
        let indent = "  ".repeat(depth);
        write!(out, "{indent}{}", row.kind).unwrap();
        // A message repeats its conversation's name; a document or a
        // container is the one whose name it is.
        let roles = self.roles.get(&row.uuid);
        if row.is_document || roles.is_some() {
            if let Some(name) = row.name.as_deref().filter(|n| !n.is_empty()) {
                write!(out, " {name:?}").unwrap();
            }
        }
        write!(out, "  {}", row.uuid).unwrap();
        if let Some(roles) = roles {
            if roles.read_together {
                out.push_str(" [read]");
            }
            if roles.is_place {
                out.push_str(" [place]");
            }
        }
        out.push('\n');
        for under in self.filed_under.get(&row.uuid).into_iter().flatten() {
            let kind = self
                .rows
                .get(under)
                .map_or("not a row", |r| r.kind.as_str());
            writeln!(out, "{indent}  also under {under} ({kind})").unwrap();
        }
        for child in self.children.get(&row.uuid).into_iter().flatten() {
            let child = &self.rows[child];
            // Another source's child is drawn in that source's tree.
            if child.source_id == row.source_id {
                self.draw(child, depth + 1, out);
            }
        }
    }
}

async fn check(path: &Path, prefix: &str) {
    let tree = Tree::read(path).await;
    let broken = tree.broken();
    assert!(
        broken.is_empty(),
        "{} rows break the tree's rules:\n{}",
        broken.len(),
        broken.join("\n")
    );
    let sources = tree.render();
    assert!(!sources.is_empty(), "the index holds rows");
    for (source_id, text) in sources {
        insta::assert_snapshot!(format!("{prefix}__{source_id}"), text);
    }
}

#[tokio::test]
async fn every_row_sits_where_the_rules_say() {
    check(&fixture_db_path(), "fixture_tree").await;
}

#[tokio::test]
async fn every_lightroom_row_sits_where_the_rules_say() {
    check(&lightroom_index_path(), "lightroom_tree").await;
}
