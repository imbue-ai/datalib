//! Where a photo sits: in its folder, under the catalog's Folders, read on
//! its own; and filed under each keyword and collection it has, each a
//! place under the keywords or collection sets above it, under the
//! catalog's Keywords or Collections. Those three are places of their own,
//! as Lightroom's Library panel lists them. Keywords and collections are
//! keyed by the catalog's `id_local`, and a folder by its `id_global`,
//! which a rename or a move inside Lightroom keeps; a folder level the
//! catalog has no row for, by its path.

use datalib_id::{IdNamespace, Identity};
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;

use crate::read::{folder_levels, ImageRow};

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

/// A folder the catalog has no row for, by its root folder's path and its
/// path from that root: a rename there is a new node.
pub fn folder_uuid(source_id: &str, root: &str, from_root: &str) -> String {
    mint(source_id, KIND_FOLDER, format!("{root}\u{0}{from_root}"))
}

pub const ROOT_FOLDER_TABLE: &str = "AgLibraryRootFolder";
pub const FOLDER_TABLE: &str = "AgLibraryFolder";

/// A root folder or a folder, by its table and its `id_global`.
pub fn folder_id_uuid(source_id: &str, table: &'static str, id_global: &str) -> String {
    mint(source_id, table, id_global.to_string())
}

/// A folder's node: by its catalog id where the catalog has one, which it
/// names as its upstream record; else by its path, naming none.
fn folder_node(
    md: &str,
    source_id: &str,
    name: &str,
    id: Option<(&'static str, &str)>,
    path: (&str, &str),
) -> DataNodeRow {
    let node = |uuid: String| {
        DataNodeRow::new(md, uuid, source_id, KIND_FOLDER_ROW)
            .named(Some(name.to_string()))
            .place()
    };
    match id {
        Some((table, id)) => node(folder_id_uuid(source_id, table, id)).upstream(table, id, None),
        None => node(folder_uuid(source_id, path.0, path.1)),
    }
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
        let root_id = image.root_id.as_deref().map(|id| (ROOT_FOLDER_TABLE, id));
        let root_node = folder_node(md, source_id, root_name(root), root_id, (root, "")).under(&at);
        at = at.below(&root_node);
        push(&mut data_nodes, root_node);
        for (i, (part, from_root)) in folder_levels(image.folder.as_deref()).enumerate() {
            let id = image
                .folder_ids
                .get(i)
                .and_then(|id| id.as_deref())
                .map(|id| (FOLDER_TABLE, id));
            let node = folder_node(md, source_id, part, id, (root, &from_root)).under(&at);
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

    /// A folder renamed or moved inside Lightroom keeps its catalog id, and
    /// so its node; a level the catalog has no row for is keyed by its path.
    #[test]
    fn a_folder_is_keyed_by_its_catalog_id_and_keeps_it_through_a_rename() {
        let image = |root: &str, folder: &str, ids: Vec<Option<&str>>| ImageRow {
            id_global: "IMG-1".into(),
            root_absolute: Some(root.into()),
            root_id: Some("ROOT-1".into()),
            folder: Some(folder.into()),
            folder_ids: ids.into_iter().map(|i| i.map(str::to_string)).collect(),
            ..ImageRow::default()
        };
        let folders = |t: &Tree| -> Vec<(String, Option<String>, Option<String>)> {
            t.data_nodes
                .iter()
                .filter(|n| n.kind == KIND_FOLDER_ROW)
                .map(|n| (n.uuid.clone(), n.name.clone(), n.upstream_id.clone()))
                .collect()
        };
        let before = of(
            "lr",
            &image(
                "/Users/picard/Pictures/",
                "2369/Risa/",
                vec![Some("F-2369"), Some("F-RISA")],
            ),
            None,
        );
        let after = of(
            "lr",
            &image(
                "/Volumes/Archive/Pictures/",
                "2369/Risa shore leave/",
                vec![Some("F-2369"), Some("F-RISA")],
            ),
            None,
        );
        let uuids = |t: &Tree| folders(t).into_iter().map(|f| f.0).collect::<Vec<_>>();
        assert_eq!(
            uuids(&before),
            uuids(&after),
            "a rename and a move keep every node"
        );
        assert_eq!(
            folders(&after)
                .into_iter()
                .map(|f| (f.1.unwrap(), f.2))
                .collect::<Vec<_>>(),
            vec![
                ("Pictures".to_string(), Some("ROOT-1".to_string())),
                ("2369".to_string(), Some("F-2369".to_string())),
                ("Risa shore leave".to_string(), Some("F-RISA".to_string())),
            ],
            "named as it is now, by the record it is"
        );
        assert_eq!(
            uuids(&before)[2],
            folder_id_uuid("lr", FOLDER_TABLE, "F-RISA")
        );

        let unlisted = of(
            "lr",
            &image(
                "/Users/picard/Pictures/",
                "2369/Risa/",
                vec![Some("F-2369"), None],
            ),
            None,
        );
        let last = folders(&unlisted).pop().unwrap();
        assert_eq!(
            last.0,
            folder_uuid("lr", "/Users/picard/Pictures/", "2369/Risa/")
        );
        assert_eq!(last.2, None, "a path-keyed folder names no upstream record");
    }

    #[test]
    fn a_root_folder_is_named_for_the_last_part_of_its_path() {
        assert_eq!(root_name("/Users/picard/Pictures/"), "Pictures");
        assert_eq!(root_name("C:\\Photos\\Risa\\"), "Risa");
        assert_eq!(root_name("/"), "/");
    }
}
