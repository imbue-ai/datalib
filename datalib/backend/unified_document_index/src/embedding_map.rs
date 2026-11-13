//! The embedding map: every embedded document placed on a plane, so that
//! documents the model embeds alike sit near each other. Written by the
//! `embedding_map` step, read by the `unified_document_index` applet and by the
//! step's next run, which starts from it.
//!
//! One JSON file, replaced whole by a rename, so a reader sees the last
//! map or the next one and never half of either. It is a cache: every
//! byte of it can be computed again from the search index, and a reset
//! deletes it, which is how a person asks for a map laid out afresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The step's tree, relative to `unified_document_index/`, named after its
/// function like every tree a step writes.
pub const DIR: &str = "embedding_map";
pub const FILE: &str = "embedding_map.json";

pub fn dir(root: &Path) -> PathBuf {
    datalib_core::layout::unified_document_index_dir(root).join(DIR)
}

pub fn path(root: &Path) -> PathBuf {
    dir(root).join(FILE)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingMap {
    /// When the step made it, as an offset-bearing stamp.
    pub made_at: String,
    pub dim: usize,
    pub epochs: usize,
    pub seed: SeedCounts,
    /// Indexed documents not embedded yet, so not on the map.
    pub unembedded: usize,
    pub points: Vec<MapPoint>,
}

/// How the run placed each point before laying it out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedCounts {
    /// Where the previous map had it.
    pub kept: usize,
    /// New, started among its neighbours from the previous map.
    pub near_neighbours: usize,
    /// Started from the corpus's principal axes.
    pub fresh: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapPoint {
    /// The document's `markdown_uuid`. A map an earlier build wrote named
    /// it by its file's path instead ([`MapPoint::markdown_uuid`]).
    #[serde(alias = "path")]
    pub document: String,
    pub x: f32,
    pub y: f32,
}

impl MapPoint {
    /// The document this point is, whether named by id or, in a map an
    /// earlier build wrote, by the path of its file.
    pub fn markdown_uuid(&self) -> &str {
        datalib_schema::grid_rows::markdown_uuid_of_path(&self.document).unwrap_or(&self.document)
    }
}

impl EmbeddingMap {
    /// Each point's position, by its document's id.
    pub fn positions(&self) -> HashMap<String, [f32; 2]> {
        self.points
            .iter()
            .map(|p| (p.markdown_uuid().to_string(), [p.x, p.y]))
            .collect()
    }
}

/// `None` when there is no map yet.
pub fn read(root: &Path) -> Result<Option<EmbeddingMap>> {
    let path = path(root);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

/// Replace the map, returning the bytes written.
pub fn write(root: &Path, map: &EmbeddingMap) -> Result<Vec<u8>> {
    let dir = dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let bytes = serde_json::to_vec(map)?;
    let dest = path(root);
    datalib_runtime::atomic::write(&dest, &bytes)
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(bytes)
}

/// Delete the map, so the next run lays one out from nothing. Nothing to
/// delete is not an error.
pub fn remove(root: &Path) -> Result<()> {
    let path = path(root);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> EmbeddingMap {
        EmbeddingMap {
            made_at: "2026-09-25T10:00:00-07:00".into(),
            dim: 768,
            epochs: 200,
            seed: SeedCounts {
                kept: 1,
                near_neighbours: 1,
                fresh: 0,
            },
            unembedded: 3,
            points: vec![
                MapPoint {
                    document: "a".into(),
                    x: 1.5,
                    y: -2.0,
                },
                MapPoint {
                    document: "b".into(),
                    x: 0.0,
                    y: 4.25,
                },
            ],
        }
    }

    #[test]
    fn a_map_round_trips_and_a_reset_removes_it() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(read(td.path()).unwrap(), None);
        write(td.path(), &map()).unwrap();
        assert_eq!(read(td.path()).unwrap(), Some(map()));
        assert_eq!(
            read(td.path()).unwrap().unwrap().positions()["b"],
            [0.0, 4.25]
        );
        remove(td.path()).unwrap();
        assert_eq!(read(td.path()).unwrap(), None);
        remove(td.path()).unwrap();
    }

    /// A map an earlier build wrote named each point by its file's path;
    /// it still reads, and its points keep their places under their ids.
    #[test]
    fn a_map_that_names_points_by_path_keeps_their_places() {
        let td = tempfile::tempdir().unwrap();
        let path = path(td.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = serde_json::json!({
            "made_at": "2026-09-25T10:00:00-07:00", "dim": 768, "epochs": 200,
            "seed": {"kept": 0, "near_neighbours": 0, "fresh": 1}, "unembedded": 0,
            "points": [{"path": "slack/render_markdown/1a/c-1a.md", "x": 1.0, "y": 2.0}],
        });
        std::fs::write(&path, old.to_string()).unwrap();
        let map = read(td.path()).unwrap().unwrap();
        assert_eq!(map.points[0].markdown_uuid(), "c-1a");
        assert_eq!(map.positions()["c-1a"], [1.0, 2.0]);
    }

    /// A reader must never see a partial file: the write goes through a
    /// temporary beside it, and nothing is left over once it lands.
    #[test]
    fn a_write_leaves_only_the_map() {
        let td = tempfile::tempdir().unwrap();
        write(td.path(), &map()).unwrap();
        let names: Vec<String> = std::fs::read_dir(dir(td.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![FILE.to_string()]);
    }
}
