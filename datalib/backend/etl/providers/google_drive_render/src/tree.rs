//! Where a Doc sits: in the folders above it, each a place, top first,
//! from My Drive or a shared drive down. A file can have several
//! parents; it sits in the first, in the order the download sorted them,
//! and is filed under the others.
//!
//! A folder is carried by every Doc in or under it, and the grid index
//! takes its name and kind from one of those Docs, so nothing a place row
//! says may come from the Doc that carries it.

use std::collections::{HashMap, HashSet};

use datalib_etl_google_drive::ingest::schema_raw::{DRIVES, FILE_PARENTS, FOLDERS};
use datalib_etl_render::inputs::Inputs;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;
use serde_json::Value;

use crate::ids;

pub const KIND_FOLDER: &str = "Folder";
pub const KIND_DRIVE: &str = "Shared drive";

/// A folder as its listing entry names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folder {
    pub name: Option<String>,
    /// The first of its `parents`; `None` at the top of what the account
    /// can see.
    pub parent: Option<String>,
}

impl Folder {
    pub fn of(entry: &Value) -> Self {
        Folder {
            name: entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string),
            parent: first_parent(entry),
        }
    }
}

/// The first of a listing entry's `parents`.
pub fn first_parent(entry: &Value) -> Option<String> {
    parents(entry).into_iter().next()
}

pub fn parents(entry: &Value) -> Vec<String> {
    entry
        .get("parents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Every folder and shared drive the store holds, by Drive's id.
#[derive(Debug, Default)]
pub struct Places {
    pub folders: HashMap<String, Folder>,
    /// A shared drive's name, by its id, which is its root folder's id.
    pub drives: HashMap<String, String>,
    /// The folders each file is in as of the newest listing, by file id.
    pub file_parents: HashMap<String, Vec<String>>,
}

/// One container above a Doc.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place<'a> {
    Folder { id: &'a str, name: Option<&'a str> },
    Drive { id: &'a str, name: &'a str },
}

impl Place<'_> {
    fn node(&self, md: &str, source_id: &str) -> DataNodeRow {
        let (uuid, kind, entity_kind, id, name) = match *self {
            Place::Folder { id, name } => (
                ids::folder(source_id, id).uuid,
                KIND_FOLDER,
                ids::KIND_FOLDER,
                id,
                name,
            ),
            Place::Drive { id, name } => (
                ids::drive(source_id, id).uuid,
                KIND_DRIVE,
                ids::KIND_DRIVE,
                id,
                Some(name),
            ),
        };
        DataNodeRow::new(md, uuid, source_id, kind)
            .named(name.map(str::to_string))
            .upstream(entity_kind, id, None)
            .place()
    }
}

impl Places {
    /// The folders `file_id` is in as the newest listing says, else as
    /// its `files` row, read at its capture, says: a store from before
    /// the download listed them has no `file_parents`.
    pub fn parents_of(&self, file_id: &str, captured: &[String], inputs: &Inputs) -> Vec<String> {
        inputs.read(FILE_PARENTS, file_id);
        self.file_parents
            .get(file_id)
            .cloned()
            .unwrap_or_else(|| captured.to_vec())
    }

    /// The containers from the top down to folder `id`, each lookup
    /// declared as a read, one that found nothing too, so a folder that
    /// arrives later renders the Doc again.
    /// An id the store holds neither as a folder nor as a drive is a
    /// folder with no name, at the top.
    fn chain<'a>(&'a self, id: &'a str, inputs: &Inputs) -> Vec<Place<'a>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut at = Some(id);
        while let Some(id) = at.filter(|id| seen.insert(*id)) {
            inputs.read(FOLDERS, id);
            if let Some(folder) = self.folders.get(id) {
                out.push(Place::Folder {
                    id,
                    name: folder.name.as_deref(),
                });
                at = folder.parent.as_deref();
                continue;
            }
            inputs.read(DRIVES, id);
            out.push(match self.drives.get(id) {
                Some(name) => Place::Drive { id, name },
                None => Place::Folder { id, name: None },
            });
            at = None;
        }
        out.reverse();
        out
    }

    /// The place rows one Doc carries, the filings of it under every
    /// parent but the first, and where its own row sits.
    pub fn tree(
        &self,
        source_id: &str,
        md: &str,
        parents: &[String],
        inputs: &Inputs,
    ) -> (Vec<DataNodeRow>, Vec<FilingRow>, Lineage) {
        let mut nodes: Vec<DataNodeRow> = Vec::new();
        let mut filings = Vec::new();
        let mut file = Lineage::top();
        for (i, parent) in parents.iter().enumerate() {
            let mut at = Lineage::top();
            for place in self.chain(parent, inputs) {
                let node = place.node(md, source_id).under(&at);
                at = at.below(&node);
                if !nodes.iter().any(|n| n.uuid == node.uuid) {
                    nodes.push(node);
                }
            }
            match (i, at.parent_uuid.as_deref()) {
                (0, _) => file = at,
                (_, Some(under)) => filings.push(FilingRow::new(md, md, under)),
                (_, None) => {}
            }
        }
        (nodes, filings, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn places() -> Places {
        let folder = |name: &str, parent: Option<&str>| Folder {
            name: Some(name.to_string()),
            parent: parent.map(str::to_string),
        };
        Places {
            folders: HashMap::from([
                ("root".into(), folder("My Drive", None)),
                ("logs".into(), folder("Captain's logs", Some("root"))),
                ("41000".into(), folder("Stardate 41000s", Some("logs"))),
                ("ready".into(), folder("Ready room", Some("root"))),
                ("eng".into(), folder("Engineering", Some("drive"))),
                ("loop".into(), folder("Loop", Some("loop"))),
            ]),
            drives: HashMap::from([("drive".into(), "Enterprise-D".into())]),
            file_parents: HashMap::new(),
        }
    }

    fn shape(nodes: &[DataNodeRow]) -> Vec<(&str, Option<&str>, Option<&str>)> {
        nodes
            .iter()
            .map(|n| (n.kind.as_str(), n.name.as_deref(), n.parent_uuid.as_deref()))
            .collect()
    }

    /// The download listed no folders, so a Doc sat in a nameless folder
    /// of one level. It sits under its whole chain now, named, top first,
    /// and is filed under its second parent.
    #[test]
    fn a_doc_sits_under_its_first_parent_and_is_filed_under_the_rest() {
        let p = places();
        let inputs = Inputs::default();
        let (nodes, filings, file) =
            p.tree("src", "md", &["41000".into(), "ready".into()], &inputs);
        let f = |id| ids::folder("src", id).uuid;
        assert_eq!(
            shape(&nodes),
            vec![
                (KIND_FOLDER, Some("My Drive"), None),
                (
                    KIND_FOLDER,
                    Some("Captain's logs"),
                    Some(f("root").as_str())
                ),
                (
                    KIND_FOLDER,
                    Some("Stardate 41000s"),
                    Some(f("logs").as_str())
                ),
                (KIND_FOLDER, Some("Ready room"), Some(f("root").as_str())),
            ]
        );
        assert!(nodes.iter().all(|n| n.is_place && !n.read_together));
        assert!(nodes.iter().all(|n| n.markdown_uuid == "md"));
        assert_eq!(
            file,
            Lineage {
                parent_uuid: Some(f("41000")),
                read_uuid: None,
                place_uuid: Some(f("41000")),
            }
        );
        let filed: Vec<(&str, &str)> = filings
            .iter()
            .map(|r| (r.node_uuid.as_str(), r.under_uuid.as_str()))
            .collect();
        assert_eq!(filed, vec![("md", f("ready").as_str())]);
        let read: Vec<String> = inputs
            .declared()
            .iter()
            .map(|i| format!("{}:{}", i.table, i.id))
            .collect();
        for id in ["41000", "logs", "root", "ready"] {
            assert!(read.contains(&format!("folders:{id}")), "{read:?}");
        }
    }

    /// A shared drive's top folders name the drive as their parent; the
    /// drive is the top place, by its name.
    #[test]
    fn a_shared_drive_is_named_at_the_top() {
        let (nodes, _, _) = places().tree("src", "md", &["eng".into()], &Inputs::default());
        let drive = ids::drive("src", "drive").uuid;
        assert_eq!(
            shape(&nodes),
            vec![
                (KIND_DRIVE, Some("Enterprise-D"), None),
                (KIND_FOLDER, Some("Engineering"), Some(drive.as_str())),
            ]
        );
    }

    /// Two Docs carry the folders they share as the same rows, whichever
    /// of their parents reached them.
    #[test]
    fn every_carrier_describes_a_folder_alike() {
        let p = places();
        let inputs = Inputs::default();
        let (a, _, _) = p.tree("src", "a", &["41000".into()], &inputs);
        let (b, _, _) = p.tree("src", "b", &["ready".into(), "logs".into()], &inputs);
        let carried = |nodes: &[DataNodeRow], uuid: &str| {
            let mut n = nodes.iter().find(|n| n.uuid == uuid).unwrap().clone();
            n.markdown_uuid.clear();
            n
        };
        for id in ["root", "logs"] {
            let uuid = ids::folder("src", id).uuid;
            assert_eq!(carried(&a, &uuid), carried(&b, &uuid), "{id}");
        }
    }

    /// A parent the store does not hold is a folder with no name at the
    /// top, and a loop in `parents` ends the chain instead of the render;
    /// a file with no parents sits at the top of the source.
    #[test]
    fn an_unknown_parent_or_a_loop_ends_the_chain() {
        let p = places();
        let inputs = Inputs::default();
        let (nodes, _, _) = p.tree("src", "md", &["gone".into()], &inputs);
        assert_eq!(shape(&nodes), vec![(KIND_FOLDER, None, None)]);
        let read: Vec<String> = inputs
            .declared()
            .iter()
            .map(|i| format!("{}:{}", i.table, i.id))
            .collect();
        assert!(read.contains(&"drives:gone".to_string()), "{read:?}");
        let (nodes, _, _) = p.tree("src", "md", &["loop".into()], &Inputs::default());
        assert_eq!(shape(&nodes), vec![(KIND_FOLDER, Some("Loop"), None)]);
        let (nodes, filings, file) = p.tree("src", "md", &[], &Inputs::default());
        assert!(nodes.is_empty() && filings.is_empty());
        assert_eq!(file, Lineage::top());
    }

    /// A file's `files` row is rewritten only when it is captured again,
    /// and a move does not call for that, so a moved Doc stayed in its
    /// old folder. The newest listing's parents win.
    #[test]
    fn a_moved_file_sits_where_the_newest_listing_puts_it() {
        let mut p = places();
        let captured = vec!["41000".to_string()];
        let inputs = Inputs::default();
        assert_eq!(p.parents_of("doc", &captured, &inputs), captured);
        assert!(inputs
            .declared()
            .iter()
            .any(|i| i.table == FILE_PARENTS && i.id == "doc"));
        p.file_parents.insert("doc".into(), vec!["ready".into()]);
        assert_eq!(
            p.parents_of("doc", &captured, &Inputs::default()),
            vec!["ready".to_string()]
        );
    }

    #[test]
    fn a_folder_is_read_from_its_listing_entry() {
        assert_eq!(
            Folder::of(&json!({"id": "f", "name": "Logs", "parents": ["a", "b"]})),
            Folder {
                name: Some("Logs".into()),
                parent: Some("a".into()),
            }
        );
        assert_eq!(Folder::of(&json!({"id": "f"})), Folder::default());
    }
}
