//! Which files `/sql` reads, named by their path under the data root. A
//! store is a `.doltlite_db` or `.sqlite` file in a step's tree, or one of
//! the few app stores under `system/` listed here: never anything else
//! under `system/`, which holds the temp files of decrypted backups, the
//! packages and the runs store's copies.

use std::path::{Path, PathBuf};

use datalib_runtime::layout;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// A doltlite store, read at one commit.
    Doltlite,
    /// A plain SQLite file, read as it is.
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub name: String,
    pub path: PathBuf,
    pub kind: StoreKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotAStore {
    /// The name is not one a store can have.
    Refused(String),
    /// No such file.
    Missing(String),
}

fn system_stores() -> [String; 6] {
    let system = layout::SYSTEM_DIR;
    [
        format!("{system}/{}/{}", layout::RUNS_DIR, layout::RUNS_DB),
        format!("{system}/{}", layout::SUPERVISOR_DB),
        format!("{system}/{}", layout::FEEDBACK_DB),
        format!("{system}/{}", layout::REMOTE_MEDIA_DB),
        format!("{system}/{}", layout::DISK_STATS_DB),
        format!("{system}/{}", layout::LEGACY_USAGE_DB),
    ]
}

/// Whether `name` may name a store, and which kind; looks at no file.
pub fn kind_of(name: &str) -> Result<StoreKind, String> {
    let components: Vec<&str> = name.split('/').collect();
    let well_formed = components
        .iter()
        .all(|c| !c.is_empty() && !c.starts_with('.') && !c.contains(['\\', '\0']));
    if !well_formed {
        return Err(format!(
            "{name:?} is not a store's name: name it by its path under the data root, \
             such as `slack/ingest/entities.doltlite_db`"
        ));
    }
    if components[0] == layout::SYSTEM_DIR && !system_stores().iter().any(|s| s == name) {
        return Err(format!(
            "{name:?} is not one of the stores under system/ this reads: {}",
            system_stores().join(", ")
        ));
    }
    if name.ends_with(".doltlite_db") {
        Ok(StoreKind::Doltlite)
    } else if name.ends_with(".sqlite") {
        Ok(StoreKind::Sqlite)
    } else {
        Err(format!(
            "{name:?} is not a store: a store's name ends in .doltlite_db or .sqlite"
        ))
    }
}

/// The store `name` names under `root`. The name is checked again as the
/// file system resolves it, so a link cannot lead out of the root or into
/// `system/`.
pub fn resolve(root: &Path, name: &str) -> Result<Store, NotAStore> {
    kind_of(name).map_err(NotAStore::Refused)?;
    let root = root
        .canonicalize()
        .map_err(|e| NotAStore::Missing(format!("the data root: {e}")))?;
    let path = root
        .join(name)
        .canonicalize()
        .map_err(|_| NotAStore::Missing(format!("no store {name:?} under the data root")))?;
    let resolved = path
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| NotAStore::Refused(format!("{name:?} leads out of the data root")))?;
    let kind = kind_of(resolved)
        .map_err(|why| NotAStore::Refused(format!("{name:?} leads to {resolved:?}, and {why}")))?;
    if !path.is_file() {
        return Err(NotAStore::Missing(format!("{name:?} is not a file")));
    }
    Ok(Store {
        name: name.to_string(),
        path,
        kind,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    pub store: String,
    pub kind: StoreKind,
    pub bytes: u64,
}

/// The stores in each step's tree (`<group>/<step>/<file>`) and the ones
/// under `system/` this reads. A store deeper in a
/// step's tree reads by name but is not listed: a render tree can hold a
/// directory per document, and listing would walk them all.
pub fn list(root: &Path) -> Vec<Listed> {
    let mut names: Vec<String> = system_stores().to_vec();
    for group in entries(root) {
        if group == layout::SYSTEM_DIR {
            continue;
        }
        for step in entries(&root.join(&group)) {
            for file in entries(&root.join(&group).join(&step)) {
                names.push(format!("{group}/{step}/{file}"));
            }
        }
    }
    let mut listed: Vec<Listed> = names
        .iter()
        .filter_map(|name| resolve(root, name).ok())
        .map(|store| Listed {
            bytes: std::fs::metadata(&store.path).map_or(0, |m| m.len()),
            store: store.name,
            kind: store.kind,
        })
        .collect();
    listed.sort_by(|a, b| a.store.cmp(&b.store));
    listed.dedup_by(|a, b| a.store == b.store);
    listed
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_is_a_database_file_in_a_step_tree_or_a_named_system_store() {
        for (name, kind) in [
            ("slack/ingest/entities.doltlite_db", StoreKind::Doltlite),
            (
                "unified_document_index/grid_index/db.sqlite",
                StoreKind::Sqlite,
            ),
            (
                "unified_document_index/search_index/inference_cache.sqlite",
                StoreKind::Sqlite,
            ),
            ("system/runs/runs.sqlite", StoreKind::Sqlite),
            ("system/supervisor.sqlite", StoreKind::Sqlite),
        ] {
            assert_eq!(kind_of(name), Ok(kind), "{name}");
        }
        for name in [
            "",
            "/etc/x.sqlite",
            "../x.sqlite",
            "slack/../system/tmp/x.sqlite",
            "slack//entities.doltlite_db",
            "slack/./ingest/entities.doltlite_db",
            "slack/ingest/.entities.doltlite_db-lock",
            "slack/ingest/entities.doltlite_db-wal",
            "system/api-token",
            "system/tmp/whatsapp/msgstore.sqlite",
            "system/runs/runs.bak_20260101.sqlite",
            "slack/ingest/notes.txt",
        ] {
            assert!(kind_of(name).is_err(), "{name:?} should be refused");
        }
    }

    /// A link is judged by where it leads: one out of the root, and one
    /// from a step's tree into `system/tmp`, are both refused.
    #[test]
    fn a_link_is_judged_by_where_it_leads() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.sqlite"), b"").unwrap();
        let root = tempfile::tempdir().unwrap();
        let step = root.path().join("g/s");
        std::fs::create_dir_all(&step).unwrap();
        std::fs::create_dir_all(root.path().join("system/tmp")).unwrap();
        std::fs::write(root.path().join("system/tmp/secret.sqlite"), b"").unwrap();
        std::os::unix::fs::symlink(outside.path().join("x.sqlite"), step.join("out.sqlite"))
            .unwrap();
        std::os::unix::fs::symlink(
            root.path().join("system/tmp/secret.sqlite"),
            step.join("in.sqlite"),
        )
        .unwrap();
        std::fs::write(step.join("ok.sqlite"), b"").unwrap();

        assert!(matches!(
            resolve(root.path(), "g/s/out.sqlite"),
            Err(NotAStore::Refused(_))
        ));
        assert!(matches!(
            resolve(root.path(), "g/s/in.sqlite"),
            Err(NotAStore::Refused(_))
        ));
        assert!(matches!(
            resolve(root.path(), "g/s/absent.sqlite"),
            Err(NotAStore::Missing(_))
        ));
        assert_eq!(
            resolve(root.path(), "g/s/ok.sqlite").unwrap().kind,
            StoreKind::Sqlite
        );
    }

    #[test]
    fn the_listing_holds_step_stores_and_named_system_stores_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for file in [
            "slack/ingest/entities.doltlite_db",
            "slack/ingest/blobs.sqlite",
            "slack/ingest/entities.doltlite_db-wal",
            "slack/render_markdown/2026/01/deep.sqlite",
            "system/runs/runs.sqlite",
            "system/tmp/decrypted.sqlite",
            "unified_document_index/search_index/inference_cache.sqlite",
        ] {
            std::fs::create_dir_all(r.join(file).parent().unwrap()).unwrap();
            std::fs::write(r.join(file), b"x").unwrap();
        }
        let names: Vec<String> = list(r).into_iter().map(|l| l.store).collect();
        assert_eq!(
            names,
            [
                "slack/ingest/blobs.sqlite",
                "slack/ingest/entities.doltlite_db",
                "system/runs/runs.sqlite",
                "unified_document_index/search_index/inference_cache.sqlite",
            ]
        );
    }
}
