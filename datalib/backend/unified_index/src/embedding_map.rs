//! The embedding map: every embedded document placed on a plane, so that
//! documents qmd embeds alike sit near each other. Written by the
//! `embedding_map` step, read by the `unified_index` applet and by the
//! step's next run, which starts from it.
//!
//! One JSON file, replaced whole by a rename, so a reader sees the last
//! map or the next one and never half of either. It is a cache: every
//! byte of it can be computed again from the qmd index, and a reset
//! deletes it, which is how a person asks for a map laid out afresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The step's tree, relative to `unified_index/`, named after its
/// function like every tree a step writes.
pub const DIR: &str = "embedding_map";
pub const FILE: &str = "embedding_map.json";

pub fn dir(root: &Path) -> PathBuf {
    datalib_core::layout::unified_index_dir(root).join(DIR)
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
    /// Active documents qmd has not embedded yet, so not on the map.
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
    /// The document's path under the data root, as qmd and the grid's
    /// `qmd_path` both name it.
    pub path: String,
    pub x: f32,
    pub y: f32,
}

impl EmbeddingMap {
    pub fn positions(&self) -> HashMap<String, [f32; 2]> {
        self.points
            .iter()
            .map(|p| (p.path.clone(), [p.x, p.y]))
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
    let tmp = dir.join(format!(".{FILE}.tmp"));
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    let dest = path(root);
    std::fs::rename(&tmp, &dest).with_context(|| format!("rename onto {}", dest.display()))?;
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
                    path: "slack/render_markdown/a.md".into(),
                    x: 1.5,
                    y: -2.0,
                },
                MapPoint {
                    path: "gmail/render_markdown/b.md".into(),
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
            read(td.path()).unwrap().unwrap().positions()["gmail/render_markdown/b.md"],
            [0.0, 4.25]
        );
        remove(td.path()).unwrap();
        assert_eq!(read(td.path()).unwrap(), None);
        remove(td.path()).unwrap();
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
