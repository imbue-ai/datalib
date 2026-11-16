//! Where a document sits: the work is a place, and each edition a place
//! inside it; a chapter of an edition is read together inside its
//! edition, and its sections inside it. A book's index page is read
//! together inside the work, since no edition owns it. A sentence is an
//! anchor inside a section, with no row: the alignment edges point at it.

use datalib_etl_perseus::WORK_TITLE;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::grid_rows::GridRow;

use super::parse::Edition;
use crate::ids;

fn identified(md: &str, id: datalib_id::Identity, source_id: &str, kind: &str) -> DataNodeRow {
    DataNodeRow::new(md, id.uuid, source_id, kind).upstream(id.entity_kind, id.natural_key, None)
}

fn work(md: &str, source_id: &str) -> DataNodeRow {
    identified(md, ids::work(source_id), source_id, "Work")
        .named(Some(WORK_TITLE.to_string()))
        .place()
}

/// A book's index: the work, then the book read together.
pub(super) fn book(source_id: &str, book_n: &str, md: &str, title: &str) -> Vec<DataNodeRow> {
    let work = work(md, source_id);
    let own = identified(md, ids::book(source_id, book_n), source_id, "Book")
        .named(Some(title.to_string()))
        .read_together()
        .under(&Lineage::top().below(&work));
    vec![work, own]
}

/// A chapter of one edition: the work, the edition, then the chapter
/// read together.
pub(super) fn chapter(
    source_id: &str,
    edition: &Edition,
    id: datalib_id::Identity,
    title: &str,
) -> Vec<DataNodeRow> {
    let md = id.uuid.clone();
    let work = work(&md, source_id);
    let in_work = Lineage::top().below(&work);
    let ed = identified(
        &md,
        ids::edition(source_id, &edition.id),
        source_id,
        "Edition",
    )
    .named(Some(edition.title.clone()))
    .place()
    .under(&in_work);
    let own = identified(&md, id, source_id, "Chapter")
        .named(Some(title.to_string()))
        .read_together()
        .under(&in_work.below(&ed));
    vec![work, ed, own]
}

/// The document's own row where its container sits (the last of
/// `data_nodes`), every other row inside it.
pub(super) fn place(rows: &mut [GridRow], data_nodes: &[DataNodeRow]) {
    let Some(own) = data_nodes.last() else {
        return;
    };
    let mut at = Lineage::top();
    for n in &data_nodes[..data_nodes.len() - 1] {
        at = at.below(n);
    }
    let inside = at.below(own);
    for row in rows {
        let l = if row.uuid == own.uuid { &at } else { &inside };
        row.parent_uuid = l.parent_uuid.clone();
        row.read_uuid = l.read_uuid.clone();
        row.place_uuid = l.place_uuid.clone();
    }
}
