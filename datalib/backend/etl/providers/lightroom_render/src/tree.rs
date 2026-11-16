//! Where a photo sits: in its catalog, a place, read on its own; and
//! filed under each keyword and collection it has, each a place under the
//! keywords or collection sets above it. Keywords and collections are
//! keyed by the catalog's `id_local`, which a rename keeps.

use datalib_id::{IdNamespace, Identity};
use datalib_schema::filings::FilingRow;
use datalib_schema::nodes::{Lineage, NodeRow};

use crate::read::ImageRow;

pub const KIND_CATALOG: &str = "catalog";

/// The source mirrors one catalog, and the catalog keeps no id of its
/// own, so the source is its key.
pub fn catalog_uuid(source_id: &str) -> String {
    mint(source_id, KIND_CATALOG, source_id.to_string())
}

/// A keyword or collection, by its table and `id_local`.
pub fn filed_uuid(source_id: &str, table: &'static str, id_local: i64) -> String {
    mint(source_id, table, id_local.to_string())
}

fn mint(source_id: &str, kind: &'static str, key: String) -> String {
    Identity::mint(IdNamespace::Lightroom, source_id, None, kind, key, None).uuid
}

fn kind_of(table: &str) -> &'static str {
    match table {
        "AgLibraryKeyword" => "Lightroom keyword",
        "AgLibraryPublishedCollection" => "Lightroom published collection",
        _ => "Lightroom collection",
    }
}

pub struct Tree {
    pub nodes: Vec<NodeRow>,
    pub filings: Vec<FilingRow>,
    /// Where the photo's own row sits.
    pub photo: Lineage,
}

pub fn of(source_id: &str, image: &ImageRow, catalog_name: Option<&str>) -> Tree {
    let md = &crate::photo::uuid(source_id, &image.id_global);
    let catalog = NodeRow::new(md, catalog_uuid(source_id), source_id, "Lightroom catalog")
        .named(catalog_name.map(String::from))
        .place();
    let in_catalog = Lineage::top().below(&catalog);
    let own = NodeRow::new(md, md, source_id, "Lightroom photo")
        .upstream(crate::photo::ENTITY_KIND, image.id_global.clone(), None)
        .read_together()
        .under(&in_catalog);
    let mut nodes = vec![catalog, own];
    let mut filings = Vec::with_capacity(image.filed.len());
    for f in &image.filed {
        let mut at = Lineage::top();
        for (id, name) in &f.path {
            let node = NodeRow::new(
                md,
                filed_uuid(source_id, f.table, *id),
                source_id,
                kind_of(f.table),
            )
            .named(Some(name.clone()))
            .upstream(f.table, id.to_string(), None)
            .place()
            .under(&at);
            at = at.below(&node);
            if !nodes.iter().any(|n| n.uuid == node.uuid) {
                nodes.push(node);
            }
        }
        if let Some(under) = &at.parent_uuid {
            filings.push(FilingRow::new(md, md, under));
        }
    }
    Tree {
        nodes,
        filings,
        photo: in_catalog,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::Filed;

    /// A photo sits in its catalog and is filed under its keyword and its
    /// collection, each placed under what is above it; a keyword two
    /// photos share is one container, keyed by its id and not its name.
    #[test]
    fn a_photo_is_filed_under_its_keywords_and_collections() {
        let image = ImageRow {
            id_global: "IMG-1".into(),
            filed: vec![
                Filed {
                    table: "AgLibraryKeyword",
                    path: vec![(1, "Places".into()), (2, "Risa".into())],
                },
                Filed {
                    table: "AgLibraryCollection",
                    path: vec![(1, "Shore leave".into()), (3, "Beach".into())],
                },
            ],
            ..ImageRow::default()
        };
        let t = of("lr", &image, Some("Enterprise"));
        let photo = crate::photo::uuid("lr", "IMG-1");
        let catalog = catalog_uuid("lr");
        let kw = |id| filed_uuid("lr", "AgLibraryKeyword", id);
        let col = |id| filed_uuid("lr", "AgLibraryCollection", id);
        assert_ne!(kw(1), col(1), "the table separates one id_local");
        let nodes: Vec<(String, Option<String>, bool, bool)> = t
            .nodes
            .iter()
            .map(|n| {
                (
                    n.uuid.clone(),
                    n.parent_uuid.clone(),
                    n.is_place,
                    n.read_together,
                )
            })
            .collect();
        assert_eq!(
            nodes,
            vec![
                (catalog.clone(), None, true, false),
                (photo.clone(), Some(catalog.clone()), false, true),
                (kw(1), None, true, false),
                (kw(2), Some(kw(1)), true, false),
                (col(1), None, true, false),
                (col(3), Some(col(1)), true, false),
            ]
        );
        let filings: Vec<(&str, &str)> = t
            .filings
            .iter()
            .map(|f| (f.node_uuid.as_str(), f.under_uuid.as_str()))
            .collect();
        assert_eq!(
            filings,
            vec![
                (photo.as_str(), kw(2).as_str()),
                (photo.as_str(), col(3).as_str())
            ]
        );
        assert!(t.filings.iter().all(|f| f.markdown_uuid == photo));
        assert_eq!(t.photo.place_uuid, Some(catalog));
    }
}
