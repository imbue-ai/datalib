//! Where a photo sits: in its folder, under the catalog's Folders, read on
//! its own; and filed under each keyword and collection it has, each a
//! place under the keywords or collection sets above it, under the
//! catalog's Keywords or Collections. Those three are places of their own,
//! as Lightroom's Library panel lists them. Keywords and collections are
//! keyed by the catalog's `id_local`, which a rename keeps; a folder by its
//! path, which is what the catalog names it by.

use datalib_id::{IdNamespace, Identity};
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;

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

/// One of the catalog's three sections, by the key it is minted from.
pub fn section_uuid(source_id: &str, section: Section) -> String {
    mint(source_id, KIND_SECTION, section.key().to_string())
}

/// A folder, by its root folder's path and its path from that root.
pub fn folder_uuid(source_id: &str, root: &str, from_root: &str) -> String {
    mint(source_id, KIND_FOLDER, format!("{root}\u{0}{from_root}"))
}

const KIND_SECTION: &str = "section";
const KIND_FOLDER: &str = "folder";

/// The places a catalog lists what it holds under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Folders,
    Keywords,
    Collections,
}

impl Section {
    fn key(self) -> &'static str {
        match self {
            Section::Folders => "folders",
            Section::Keywords => "keywords",
            Section::Collections => "collections",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Section::Folders => "Folders",
            Section::Keywords => "Keywords",
            Section::Collections => "Collections",
        }
    }
}

fn kind_of(table: &str) -> &'static str {
    match table {
        "AgLibraryKeyword" => "Lightroom keyword",
        "AgLibraryPublishedCollection" => "Lightroom published collection",
        _ => "Lightroom collection",
    }
}

fn section_of(table: &str) -> Section {
    match table {
        "AgLibraryKeyword" => Section::Keywords,
        _ => Section::Collections,
    }
}

/// A root folder is named for the last part of its path, as Lightroom
/// shows it, whichever separator the catalog's machine used.
fn root_name(root: &str) -> &str {
    root.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(root)
}

pub struct Tree {
    pub data_nodes: Vec<DataNodeRow>,
    pub filings: Vec<FilingRow>,
    /// Where the photo's own row sits.
    pub photo: Lineage,
}

pub fn of(source_id: &str, image: &ImageRow, catalog_name: Option<&str>) -> Tree {
    let md = &crate::photo::uuid(source_id, &image.id_global);
    let catalog = DataNodeRow::new(md, catalog_uuid(source_id), source_id, "Lightroom catalog")
        .named(catalog_name.map(String::from))
        .place();
    let in_catalog = Lineage::top().below(&catalog);
    let mut data_nodes = vec![catalog];
    let push = |nodes: &mut Vec<DataNodeRow>, node: DataNodeRow| {
        if !nodes.iter().any(|n| n.uuid == node.uuid) {
            nodes.push(node);
        }
    };
    let section = |s: Section| {
        DataNodeRow::new(
            md,
            section_uuid(source_id, s),
            source_id,
            "Lightroom section",
        )
        .named(Some(s.name().to_string()))
        .place()
        .under(&in_catalog)
    };

    let mut photo = in_catalog.clone();
    if let Some(root) = image.root_absolute.as_deref().filter(|r| !r.is_empty()) {
        let folders = section(Section::Folders);
        let mut at = in_catalog.below(&folders);
        push(&mut data_nodes, folders);
        let root_node = DataNodeRow::new(
            md,
            folder_uuid(source_id, root, ""),
            source_id,
            KIND_FOLDER_ROW,
        )
        .named(Some(root_name(root).to_string()))
        .place()
        .under(&at);
        at = at.below(&root_node);
        push(&mut data_nodes, root_node);
        let mut from_root = String::new();
        for part in image
            .folder
            .as_deref()
            .unwrap_or("")
            .split('/')
            .filter(|p| !p.is_empty())
        {
            from_root.push_str(part);
            from_root.push('/');
            let node = DataNodeRow::new(
                md,
                folder_uuid(source_id, root, &from_root),
                source_id,
                KIND_FOLDER_ROW,
            )
            .named(Some(part.to_string()))
            .place()
            .under(&at);
            at = at.below(&node);
            push(&mut data_nodes, node);
        }
        photo = at;
    }
    let own = DataNodeRow::new(md, md, source_id, "Lightroom photo")
        .upstream(crate::photo::ENTITY_KIND, image.id_global.clone(), None)
        .read_together()
        .under(&photo);
    data_nodes.push(own);

    let mut filings = Vec::with_capacity(image.filed.len());
    for f in &image.filed {
        let top = section(section_of(f.table));
        let mut at = in_catalog.below(&top);
        push(&mut data_nodes, top);
        for (id, name) in &f.path {
            let node = DataNodeRow::new(
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
            push(&mut data_nodes, node);
        }
        if let Some(under) = &at.parent_uuid {
            filings.push(FilingRow::new(md, md, under));
        }
    }
    Tree {
        data_nodes,
        filings,
        photo,
    }
}

const KIND_FOLDER_ROW: &str = "Lightroom folder";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::Filed;

    /// A photo with no folder sits in its catalog and is filed under its
    /// keyword and its collection, each placed under what is above it and,
    /// at the top, under the catalog's Keywords or Collections; a keyword
    /// two photos share is one container, keyed by its id and not its name.
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
        let data_nodes: Vec<(String, Option<String>, bool, bool)> = t
            .data_nodes
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
        let keywords = section_uuid("lr", Section::Keywords);
        let collections = section_uuid("lr", Section::Collections);
        assert_eq!(
            data_nodes,
            vec![
                (catalog.clone(), None, true, false),
                (photo.clone(), Some(catalog.clone()), false, true),
                (keywords.clone(), Some(catalog.clone()), true, false),
                (kw(1), Some(keywords), true, false),
                (kw(2), Some(kw(1)), true, false),
                (collections.clone(), Some(catalog.clone()), true, false),
                (col(1), Some(collections), true, false),
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

    /// A photo sits in its folder: the catalog's Folders, its root folder
    /// named for the last part of its path, then each folder of its path
    /// from that root. Two photos in one folder share it.
    #[test]
    fn a_photo_sits_in_its_folder_under_the_catalogs_folders() {
        let image = |id: &str| ImageRow {
            id_global: id.into(),
            root_absolute: Some("/Users/picard/Pictures/Away missions/".into()),
            folder: Some("2369/Risa/".into()),
            ..ImageRow::default()
        };
        let a = of("lr", &image("IMG-1"), None);
        let b = of("lr", &image("IMG-2"), None);
        let catalog = catalog_uuid("lr");
        let folders = section_uuid("lr", Section::Folders);
        let root = "/Users/picard/Pictures/Away missions/";
        let chain: Vec<(Option<String>, Option<String>, Option<String>)> = a
            .data_nodes
            .iter()
            .filter(|n| n.kind == KIND_FOLDER_ROW || n.uuid == folders)
            .map(|n| (Some(n.uuid.clone()), n.name.clone(), n.parent_uuid.clone()))
            .collect();
        assert_eq!(
            chain,
            vec![
                (Some(folders.clone()), Some("Folders".into()), Some(catalog)),
                (
                    Some(folder_uuid("lr", root, "")),
                    Some("Away missions".into()),
                    Some(folders),
                ),
                (
                    Some(folder_uuid("lr", root, "2369/")),
                    Some("2369".into()),
                    Some(folder_uuid("lr", root, "")),
                ),
                (
                    Some(folder_uuid("lr", root, "2369/Risa/")),
                    Some("Risa".into()),
                    Some(folder_uuid("lr", root, "2369/")),
                ),
            ]
        );
        let risa = folder_uuid("lr", root, "2369/Risa/");
        assert_eq!(a.photo.parent_uuid.as_deref(), Some(risa.as_str()));
        assert_eq!(a.photo.place_uuid.as_deref(), Some(risa.as_str()));
        assert_eq!(b.photo, a.photo, "two photos in one folder share it");
    }

    #[test]
    fn a_root_folder_is_named_for_the_last_part_of_its_path() {
        assert_eq!(root_name("/Users/picard/Pictures/"), "Pictures");
        assert_eq!(root_name("C:\\Photos\\Risa\\"), "Risa");
        assert_eq!(root_name("/"), "/");
    }
}
