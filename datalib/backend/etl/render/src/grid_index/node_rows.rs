//! A grid row for every data node that has none of its own: a channel, a
//! label, a project, most places. A node that is also a document's row (a
//! thread, a chat) already has one and is left to that document. The rest
//! are written here from the `data_nodes` the documents carry, so every
//! node of the tree can be found, linked and listed like any row. Such a
//! row belongs to no document (`markdown_uuid` is null) and goes when the
//! last document that carries its node does.

use anyhow::{Context, Result};
use datalib_schema::grid_rows::{filed_under, GridRow};
use datalib_schema::providers::Provider;

use super::{put_grid_row, WriteLock};

/// How the index derives the node rows. It is part of the grid index's
/// shape, as the terms' is, and an index in another shape is built again:
/// change it whenever what a node row holds, or which nodes get one,
/// changes.
pub(super) const NODE_ROWS_SHAPE: &str = "1";

/// One carrier's account of a node that no document holds a row for, with
/// what that carrier's document row says of the source.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(super) struct CarriedNode {
    pub uuid: String,
    pub source_id: String,
    pub kind: String,
    pub name: Option<String>,
    pub parent_uuid: Option<String>,
    pub sort_key: String,
    pub read_uuid: Option<String>,
    pub place_uuid: Option<String>,
    pub upstream_entity_kind: Option<String>,
    pub upstream_id: Option<String>,
    pub upstream_account: Option<String>,
    pub provider: String,
    pub source_label: String,
}

/// What [`CARRIED_WITHOUT_A_ROW`] reads one source's nodes by, and names.
/// Without it SQLite started from every document row in the index, so
/// each source cost a read of the whole index on every pass. Out of
/// `grid_index::schema_hash`, as `AUTHORS_INDEX` is: it changes no row,
/// and adding it must not rebuild the index.
pub(super) const BY_SOURCE_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS data_nodes_by_source ON data_nodes (source_id)";

/// The nodes a source's documents carry that no document holds a row for,
/// each as its newest carrier tells it: the carrier with the greatest
/// `markdown_uuid`, whose stamp-led ids make that the latest. A node whose
/// uuid is already a row of some document is skipped.
pub(super) const CARRIED_WITHOUT_A_ROW: &str = "\
    SELECT n.uuid, n.source_id, n.kind, n.name, n.parent_uuid, n.sort_key, \
           n.read_uuid, n.place_uuid, n.upstream_entity_kind, n.upstream_id, \
           n.upstream_account, d.provider, d.source_label, max(n.markdown_uuid) \
    FROM data_nodes n INDEXED BY data_nodes_by_source \
    JOIN grid_rows d ON d.markdown_uuid = n.markdown_uuid AND d.is_document = 1 \
    WHERE n.source_id = ? \
      AND NOT EXISTS (SELECT 1 FROM grid_rows g \
                      WHERE g.uuid = n.uuid AND g.markdown_uuid IS NOT NULL) \
    GROUP BY n.uuid";

/// Node rows no document carries any more, in every source at once: there
/// are few of them, and a node filed under datalib is not under its
/// source's id.
pub(super) const ORPHANED: &str = "\
    DELETE FROM grid_rows \
    WHERE markdown_uuid IS NULL \
      AND NOT EXISTS (SELECT 1 FROM data_nodes n WHERE n.uuid = grid_rows.uuid) \
    RETURNING uuid";

/// The row a node is shown as. A node has no author and no time of its
/// own, so those stay empty; its name is its title. `None` for a provider
/// tag this build does not know, which its carrier's own rows could not
/// have been written with either.
pub(super) fn node_row(node: &CarriedNode) -> Option<GridRow> {
    let provider = Provider::parse(&node.provider)?;
    let row = GridRow::builder()
        .uuid(node.uuid.clone())
        .provider(provider)
        .kind(node.kind.clone())
        .source_label(node.source_label.clone())
        .conversation_uuid(node.uuid.clone())
        .conversation_name(node.name.clone())
        .parent_uuid(node.parent_uuid.clone())
        .sort_key(Some(node.sort_key.clone()))
        .read_uuid(node.read_uuid.clone())
        .place_uuid(node.place_uuid.clone())
        .upstream_entity_kind(node.upstream_entity_kind.clone())
        .upstream_id(node.upstream_id.clone())
        .upstream_account(node.upstream_account.clone())
        .build()
        .ok()?;
    Some(GridRow {
        source_id: Some(filed_under(&node.provider, &node.source_id)),
        ..row
    })
}

/// Bring the node rows in line with what the documents now carry: write
/// one for each of `source_id`'s nodes that has no row, refresh those
/// whose carrier says something new, and drop every node row no document
/// carries. Returns how many of the source's nodes have a row of their
/// own here, and how many rows it dropped.
pub(super) async fn bring_in_line(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    write_lock: &WriteLock,
    source_id: &str,
) -> Result<(usize, usize)> {
    let carried: Vec<CarriedNode> = sqlx::query_as(CARRIED_WITHOUT_A_ROW)
        .bind(source_id)
        .fetch_all(&mut **conn)
        .await
        .with_context(|| format!("read {source_id}'s nodes that have no row"))?;
    let mut kept = 0;
    for node in &carried {
        let Some(row) = node_row(node) else {
            tracing::warn!(
                node = %node.uuid,
                provider = %node.provider,
                "a node whose carrier names a provider this build does not know; no row for it"
            );
            continue;
        };
        put_grid_row(conn, write_lock, &row).await?;
        kept += 1;
    }
    let dropped: Vec<String> = sqlx::query_scalar(ORPHANED)
        .fetch_all(&mut **conn)
        .await
        .context("drop the node rows no document carries")?;
    let count = dropped.len();
    write_lock.note_changed(dropped);
    Ok((kept, count))
}
