//! `nodes`, one row per data node, settled from what the documents say of
//! each (`data_nodes`); and a grid row for every data node that has none
//! of its own: a channel, a label, a project, most places. A node that is
//! also a document's row (a thread, a chat) already has one and is left to
//! that document. The rest are written here, so every node of the tree can
//! be found, linked and listed like any row. Such a row belongs to no
//! document (`markdown_uuid` is null). A node, and a row written for one,
//! go when the last document that carries the node does.
//!
//! Only the nodes a pass touched are settled ([`super::WriteLock`] keeps
//! them): a node whose accounts changed, or whose grid row moved between
//! documents, which is what [`SETTLE`] chooses by.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use datalib_etl::bulk::BulkUpsertable;
use datalib_schema::grid_rows::{filed_under, GridRow};
use datalib_schema::nodes::NodeRow;
use datalib_schema::providers::Provider;

use super::{put_grid_row, WriteLock};

/// How the index derives the nodes and their rows. It is part of the grid
/// index's shape, as the terms' is, and an index in another shape is built
/// again: change it whenever what a node or its row holds, or which nodes
/// get a row, changes.
pub(super) const NODES_SHAPE: &str = "2";

/// How many nodes one statement settles, its uuids bound as one array.
const AT_ONCE: usize = 4_000;

/// Writes each node of the JSON array `?` as one account of it tells it:
/// the document that holds the node's own grid row, which knows it best,
/// else its newest, the one with the greatest `markdown_uuid`, whose
/// stamp-led ids make that the latest. A node already so is not written.
pub(super) static SETTLE: LazyLock<String> = LazyLock::new(|| {
    let columns: Vec<&str> = std::iter::once(NodeRow::ID_COLUMN)
        .chain(NodeRow::TYPED_COLUMNS.iter().copied())
        .collect();
    let each = |prefix: &str| {
        columns
            .iter()
            .map(|c| format!("{prefix}{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let set: Vec<String> = columns
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    format!(
        "INSERT INTO nodes ({}) \
         SELECT {} FROM json_each(?) CROSS JOIN data_nodes n \
            ON n.uuid = json_each.value AND n.markdown_uuid = COALESCE(\
               (SELECT d.markdown_uuid FROM grid_rows g CROSS JOIN data_nodes d \
                   ON d.markdown_uuid = g.markdown_uuid AND d.uuid = g.uuid \
                WHERE g.uuid = json_each.value), \
               (SELECT max(d.markdown_uuid) FROM data_nodes d INDEXED BY data_nodes_by_uuid \
                WHERE d.uuid = json_each.value)) \
         WHERE true \
         ON CONFLICT (uuid) DO UPDATE SET {} WHERE ({}) IS NOT ({})",
        each(""),
        each("n."),
        set.join(", "),
        each("nodes."),
        each("excluded."),
    )
});

/// The nodes of the JSON array `?` no document carries any more.
pub(super) const UNCARRIED: &str = "\
    DELETE FROM nodes \
    WHERE uuid IN (SELECT value FROM json_each(?)) \
      AND NOT EXISTS (SELECT 1 FROM data_nodes n INDEXED BY data_nodes_by_uuid \
                      WHERE n.uuid = nodes.uuid)";

/// The rows written for nodes of the JSON array `?` that are gone.
pub(super) const ORPHANED: &str = "\
    DELETE FROM grid_rows \
    WHERE uuid IN (SELECT value FROM json_each(?)) \
      AND markdown_uuid IS NULL \
      AND NOT EXISTS (SELECT 1 FROM nodes n WHERE n.uuid = grid_rows.uuid) \
    RETURNING uuid";

/// The nodes of the JSON array `?` no document holds a row for, with what
/// the document row of each one's newest account says of the source.
pub(super) const WITHOUT_A_ROW: &str = "\
    SELECT n.*, d.provider, d.source_label \
    FROM json_each(?) CROSS JOIN nodes n ON n.uuid = json_each.value \
    LEFT JOIN grid_rows d INDEXED BY grid_rows_by_markdown \
       ON d.is_document = 1 AND d.markdown_uuid = \
          (SELECT max(a.markdown_uuid) FROM data_nodes a INDEXED BY data_nodes_by_uuid \
           WHERE a.uuid = n.uuid) \
    WHERE NOT EXISTS (SELECT 1 FROM grid_rows g \
                      WHERE g.uuid = n.uuid AND g.markdown_uuid IS NOT NULL)";

/// A node no document holds a row for. The source's facts are `None` while
/// its newest account's document has lost its own row to another document
/// and not been rendered again.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(super) struct Unrowed {
    #[sqlx(flatten)]
    pub node: NodeRow,
    pub provider: Option<String>,
    pub source_label: Option<String>,
}

/// The row a node is shown as. A node has no author and no time of its
/// own, so those stay empty; its name is its title. `None` without the
/// source's facts, or for a provider tag this build does not know, which
/// its carrier's own rows could not have been written with either.
pub(super) fn node_row(unrowed: &Unrowed) -> Option<GridRow> {
    let node = &unrowed.node;
    let provider = Provider::parse(unrowed.provider.as_deref()?)?;
    let row = GridRow::builder()
        .uuid(node.uuid.clone())
        .provider(provider)
        .kind(node.kind.clone())
        .source_label(unrowed.source_label.clone()?)
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
        source_id: Some(filed_under(provider.as_str(), &node.source_id)),
        ..row
    })
}

/// What settling the touched nodes did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settled {
    pub touched: usize,
    /// Nodes written and removed.
    pub changed: u64,
    /// Touched nodes that have a row of their own here, and such rows
    /// dropped.
    pub rows_kept: usize,
    pub rows_dropped: usize,
}

/// Bring the nodes the transaction touched, and their rows, in line with
/// what the documents now carry.
pub(super) async fn settle(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    write_lock: &WriteLock,
) -> Result<Settled> {
    let touched = write_lock.take_touched_nodes();
    let mut settled = Settled {
        touched: touched.len(),
        ..Default::default()
    };
    for some in touched.chunks(AT_ONCE) {
        let set = serde_json::to_string(some)?;
        // Audited: built from `NodeRow`'s associated consts alone.
        for sql in [SETTLE.as_str(), UNCARRIED] {
            settled.changed += sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&set)
                .execute(&mut **conn)
                .await
                .with_context(|| format!("settle {} nodes: {sql}", some.len()))?
                .rows_affected();
        }
        let dropped: Vec<String> = sqlx::query_scalar(ORPHANED)
            .bind(&set)
            .fetch_all(&mut **conn)
            .await
            .context("drop the node rows no document carries")?;
        settled.rows_dropped += dropped.len();
        write_lock.note_changed(dropped);
        let unrowed: Vec<Unrowed> = sqlx::query_as(WITHOUT_A_ROW)
            .bind(&set)
            .fetch_all(&mut **conn)
            .await
            .context("read the nodes that have no row")?;
        for node in &unrowed {
            let Some(row) = node_row(node) else {
                tracing::warn!(
                    node = %node.node.uuid,
                    provider = node.provider.as_deref().unwrap_or("none"),
                    "a node whose newest carrier has no document row, or names a provider \
                     this build does not know; its own row is left as it was"
                );
                continue;
            };
            put_grid_row(conn, write_lock, &row).await?;
            settled.rows_kept += 1;
        }
    }
    Ok(settled)
}
