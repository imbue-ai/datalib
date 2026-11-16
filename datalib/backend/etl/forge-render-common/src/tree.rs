//! Where a change request sits: in its repository or project, a place,
//! and read together itself. Its own row is the description, so the
//! document's row is the container's, as a Notion page's is. Its
//! comments sit inside it, and a reply is an edge to what it answers.
//! No account, as on the rows: the login that fetched a change request
//! is not on it.

use std::collections::HashMap;

use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::edges::{EdgeKind, EdgeRow};

use crate::{ChangeRequest, Comment, ForgeProfile};

pub(crate) struct Tree {
    pub data_nodes: Vec<DataNodeRow>,
    /// Where the change request's own row sits.
    pub change_request: Lineage,
    /// Inside it: where every comment sits.
    pub inside: Lineage,
}

pub(crate) fn of(profile: &ForgeProfile, cr: &ChangeRequest, stanza: &str) -> Tree {
    let md = &cr.uuid;
    let mut data_nodes = Vec::with_capacity(2);
    let mut at = Lineage::top();
    if let Some(c) = &cr.container_node {
        let node = DataNodeRow::new(md, &c.uuid, stanza, c.kind)
            .named(Some(cr.container.clone()))
            .upstream(c.entity_kind, c.upstream_id.clone(), None)
            .place();
        at = at.below(&node);
        data_nodes.push(node);
    }
    let own = DataNodeRow::new(md, md, stanza, profile.doc_kind)
        .named(Some(cr.title.clone()))
        .upstream(
            profile.doc_entity_kind,
            datalib_id::composite_key(&[&cr.container, &cr.number.to_string()]),
            None,
        )
        .read_together()
        .under(&at);
    let inside = at.below(&own);
    data_nodes.push(own);
    Tree {
        data_nodes,
        change_request: at,
        inside,
    }
}

/// An edge from each comment to the one it replies to, when that one is
/// on the same change request. Ids are unique within one kind only.
pub(crate) fn reply_edges(md: &str, comments: &[&Comment]) -> Vec<EdgeRow> {
    let by_id: HashMap<(&str, i64), &str> = comments
        .iter()
        .map(|c| ((c.entity_kind, c.external_id), c.uuid.as_str()))
        .collect();
    comments
        .iter()
        .filter_map(|c| {
            let to = *by_id.get(&(c.entity_kind, c.in_reply_to_id?))?;
            Some(EdgeRow {
                kind: EdgeKind::ReplyTo.as_i64(),
                edge_uuid: datalib_id::edge_id(md, Some(&c.uuid), md, Some(to), None),
                src_markdown_uuid: md.to_string(),
                src_anchor_uuid: Some(c.uuid.clone()),
                dst_markdown_uuid: md.to_string(),
                dst_anchor_uuid: Some(to.to_string()),
                label: None,
            })
        })
        .collect()
}
