//! A library's tree, a level at a time: the places directly in a place, or
//! at a source's top, in `(sort_key, uuid)` order, a page past a cursor.
//! Every read is off `nodes_places`. What else is in a place is searched,
//! by [`crate::at_node`].

use datalib_sql::{sql, Sql};
use serde::Serialize;

/// Where a place sorts in its level. Also a cursor: the last place of a
/// page, never a count, so one that arrives later moves no page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Key {
    pub sort_key: String,
    pub uuid: String,
}

/// Whose places a request lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parent {
    /// A place, by uuid.
    Node(String),
    /// A configured source, by id: the places it holds at its top.
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

    /// What [`level_sql`] binds as `?1`.
    pub fn bound(&self) -> &str {
        match self {
            Parent::Node(uuid) => uuid,
            Parent::SourceTop(id) => id,
        }
    }
}

/// One place of a level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Place {
    pub uuid: String,
    pub sort_key: String,
    pub kind: String,
    pub name: Option<String>,
    /// The start of its own text: a Notion page has some, a channel none.
    pub preview: Option<String>,
    /// The document it is; `None` for a place that is none.
    pub markdown_uuid: Option<String>,
    /// Whether places lie below it.
    pub places_below: bool,
}

impl Place {
    pub fn key(&self) -> Key {
        Key {
            sort_key: self.sort_key.clone(),
            uuid: self.uuid.clone(),
        }
    }
}

/// A page of a level and the commit it was read at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlacesPage {
    pub places: Vec<Place>,
    /// Past the last place, when more follow it.
    pub after: Option<Key>,
    pub at: Option<String>,
}

/// The first `limit` of what [`level_sql`] read with a limit one more, so
/// that a list that long says there is more.
pub fn page(mut read: Vec<Place>, limit: usize, at: String) -> PlacesPage {
    let more = read.len() > limit;
    read.truncate(limit);
    PlacesPage {
        after: read.last().filter(|_| more).map(Place::key),
        places: read,
        at: Some(at),
    }
}

/// A level's places past a key, binding the parent as `?1`, the key as
/// `?2, ?3` and the limit as `?4`.
pub fn level_sql(parent: &Parent) -> String {
    let level = match parent {
        Parent::Node(_) => "n.parent_uuid = ?1",
        Parent::SourceTop(_) => "n.parent_uuid IS NULL AND n.source_id = ?1",
    };
    format!(
        "SELECT n.uuid, n.sort_key, n.kind, n.name, g.preview, g.markdown_uuid, \
             EXISTS (SELECT 1 FROM nodes b INDEXED BY nodes_places \
                     WHERE b.parent_uuid = n.uuid AND b.is_place = 1) AS places_below \
         FROM nodes n INDEXED BY nodes_places \
         LEFT JOIN grid_rows g ON g.uuid = n.uuid \
         WHERE {level} AND n.is_place = 1 AND (n.sort_key, n.uuid) > (?2, ?3) \
         ORDER BY n.sort_key, n.uuid LIMIT ?4"
    )
}

/// One node of the tree, by uuid: what a `datalib:node/…` link opens.
/// Any node, a place or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Node {
    pub uuid: String,
    pub source_id: String,
    pub kind: String,
    pub name: Option<String>,
    /// The document it is; `None` for a node that is none.
    pub markdown_uuid: Option<String>,
}

/// The node `?1` names.
pub const NODE_SQL: Sql = sql!(
    "unified_grid_and_search_index.read_node",
    "SELECT n.uuid, n.source_id, n.kind, n.name, g.markdown_uuid \
     FROM nodes n INDEXED BY sqlite_autoindex_nodes_1 \
     LEFT JOIN grid_rows g ON g.uuid = n.uuid \
     WHERE n.uuid = ?1"
);

/// The sources that hold places at their top.
pub const SOURCES_WITH_PLACES_SQL: Sql = sql!(
    "unified_grid_and_search_index.list_sources_with_places",
    "SELECT DISTINCT source_id \
     FROM nodes INDEXED BY nodes_places WHERE parent_uuid IS NULL AND is_place = 1 \
     ORDER BY source_id"
);
