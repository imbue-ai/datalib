// The containers rows live in — a chat, a thread, a channel, a label, a
// page — and what each is filed under. A container is carried by every
// document that names it, the way a person is (`source_contacts`): a
// re-render replaces its own rows, and a channel named by many threads
// has a row under each, one container when read
// (docs/dev/plans/messages_not_documents.md).

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "nodes", primary_key = "markdown_uuid, uuid")]
pub struct NodeRow {
    /// The document that carries this row.
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    /// What it is, for a person: "Slack channel", "Label", "Chat".
    #[col(sql = "VARCHAR(64)")]
    pub kind: String,
    #[col(sql = "TEXT")]
    pub name: Option<String>,
    /// The container this one lives in; null at the top of its source.
    #[col(sql = "VARCHAR(96)")]
    pub parent_uuid: Option<String>,
    /// As `grid_rows.sort_key`: how it sorts among its siblings.
    #[col(sql = "VARCHAR(40)")]
    pub sort_key: Option<String>,
    /// Its children are read together: what a search groups by and the
    /// viewer opens.
    #[col(sql = "INTEGER")]
    pub read_together: bool,
    /// A person browses or filters by it.
    #[col(sql = "INTEGER")]
    pub is_place: bool,
    /// As on `grid_rows`: the nearest containers above this one of each
    /// role.
    #[col(sql = "VARCHAR(96)")]
    pub read_uuid: Option<String>,
    #[col(sql = "VARCHAR(96)")]
    pub place_uuid: Option<String>,
    /// The upstream record it is, as on `grid_rows`.
    #[col(sql = "VARCHAR(128)")]
    pub upstream_entity_kind: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub upstream_id: Option<String>,
    #[col(sql = "VARCHAR(256)")]
    pub upstream_account: Option<String>,
}

/// Where a row or container sits: its parent and the nearest container
/// above it of each role. The top of a source is all `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lineage {
    pub parent_uuid: Option<String>,
    pub read_uuid: Option<String>,
    pub place_uuid: Option<String>,
}

impl Lineage {
    pub fn top() -> Self {
        Self::default()
    }

    /// Where the children of `node` sit.
    pub fn below(&self, node: &NodeRow) -> Self {
        let own = || Some(node.uuid.clone());
        Lineage {
            parent_uuid: own(),
            read_uuid: if node.read_together {
                own()
            } else {
                self.read_uuid.clone()
            },
            place_uuid: if node.is_place {
                own()
            } else {
                self.place_uuid.clone()
            },
        }
    }
}

impl NodeRow {
    pub fn new(
        markdown_uuid: impl Into<String>,
        uuid: impl Into<String>,
        source_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        NodeRow {
            markdown_uuid: markdown_uuid.into(),
            uuid: uuid.into(),
            source_id: source_id.into(),
            kind: kind.into(),
            name: None,
            parent_uuid: None,
            sort_key: None,
            read_together: false,
            is_place: false,
            read_uuid: None,
            place_uuid: None,
            upstream_entity_kind: None,
            upstream_id: None,
            upstream_account: None,
        }
    }

    pub fn named(mut self, name: impl Into<Option<String>>) -> Self {
        self.name = name.into();
        self
    }

    pub fn upstream(
        mut self,
        entity_kind: impl Into<String>,
        id: impl Into<String>,
        account: impl Into<Option<String>>,
    ) -> Self {
        self.upstream_entity_kind = Some(entity_kind.into());
        self.upstream_id = Some(id.into());
        self.upstream_account = account.into();
        self
    }

    pub fn read_together(mut self) -> Self {
        self.read_together = true;
        self
    }

    pub fn place(mut self) -> Self {
        self.is_place = true;
        self
    }

    pub fn sort_key(mut self, key: impl Into<Option<String>>) -> Self {
        self.sort_key = key.into();
        self
    }

    pub fn under(mut self, at: &Lineage) -> Self {
        self.parent_uuid = at.parent_uuid.clone();
        self.read_uuid = at.read_uuid.clone();
        self.place_uuid = at.place_uuid.clone();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container's children take it as parent, and as their nearest
    /// read-together or place container only when it has that role; a role
    /// it lacks passes down from above.
    #[test]
    fn lineage_passes_each_role_down_from_the_nearest_container_that_has_it() {
        let channel = NodeRow::new("md", "channel", "s", "Slack channel").place();
        let in_channel = Lineage::top().below(&channel);
        let thread = NodeRow::new("md", "thread", "s", "Slack thread")
            .read_together()
            .under(&in_channel);
        assert_eq!(thread.parent_uuid.as_deref(), Some("channel"));
        assert_eq!(thread.place_uuid.as_deref(), Some("channel"));
        assert_eq!(thread.read_uuid, None);
        let in_thread = in_channel.below(&thread);
        assert_eq!(
            in_thread,
            Lineage {
                parent_uuid: Some("thread".into()),
                read_uuid: Some("thread".into()),
                place_uuid: Some("channel".into()),
            }
        );
    }
}
