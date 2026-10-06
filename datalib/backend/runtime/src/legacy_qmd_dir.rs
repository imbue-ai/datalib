//! Moves a data root's qmd index from `unified_index/qmd_index`, where
//! builds before 0.39 kept it, to `unified_index/qmd_aggregator`, so an
//! upgraded root keeps its embeddings instead of building them again.
//!
//! Temporary: it ships for two minor releases, 0.39 and 0.40. The test
//! at the bottom fails from 0.41 on; then delete this file and its two
//! callers (`datalib_step`'s `qmd_index::open_index` and the
//! `unified_index` applet's `serve`).

use std::path::{Path, PathBuf};

const LEGACY_DIR: &str = "qmd_index";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    NothingToMove,
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    /// Both directories exist, so neither is touched: the new one is the
    /// index in use, and the old one is bytes a person can delete.
    LeftBeside {
        old: PathBuf,
    },
}

pub fn move_to_aggregator_dir(root: &Path) -> std::io::Result<Outcome> {
    let old = crate::layout::unified_index_dir(root).join(LEGACY_DIR);
    let new = crate::layout::qmd_dir(root);
    if std::fs::symlink_metadata(&old).is_err() {
        return Ok(Outcome::NothingToMove);
    }
    if std::fs::symlink_metadata(&new).is_ok() {
        return Ok(Outcome::LeftBeside { old });
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => Ok(Outcome::Moved { from: old, to: new }),
        // Every qmd step and the applet call this, so another process
        // may have moved it between the check and the rename.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Outcome::NothingToMove),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"index").unwrap();
    }

    #[test]
    fn an_old_index_moves_to_the_aggregators_directory() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write(&root.join("unified_index/qmd_index/qmd/index.sqlite"));

        let outcome = move_to_aggregator_dir(root).unwrap();

        assert!(matches!(outcome, Outcome::Moved { .. }), "{outcome:?}");
        assert!(crate::qmd::qmd_index_path(root).is_file());
        assert!(!root.join("unified_index/qmd_index").exists());
        assert_eq!(
            move_to_aggregator_dir(root).unwrap(),
            Outcome::NothingToMove
        );
    }

    /// The index in use is never replaced by an older one.
    #[test]
    fn with_both_present_neither_is_touched() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        write(&root.join("unified_index/qmd_index/qmd/index.sqlite"));
        write(&crate::qmd::qmd_index_path(root));

        let outcome = move_to_aggregator_dir(root).unwrap();

        assert!(matches!(outcome, Outcome::LeftBeside { .. }), "{outcome:?}");
        assert!(root
            .join("unified_index/qmd_index/qmd/index.sqlite")
            .is_file());
        assert!(crate::qmd::qmd_index_path(root).is_file());
    }

    #[test]
    fn a_root_without_an_old_index_is_left_alone() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(
            move_to_aggregator_dir(td.path()).unwrap(),
            Outcome::NothingToMove
        );
        assert!(!crate::layout::qmd_dir(td.path()).exists());
    }

    /// The move was promised for 0.39 and 0.40 only. When this fails,
    /// delete this file and its callers (see the header).
    #[test]
    fn the_move_is_deleted_by_0_41() {
        let mut parts = crate::build_id::DATALIB_VERSION.split('.');
        let major: u32 = parts.next().unwrap().parse().unwrap();
        let minor: u32 = parts.next().unwrap().parse().unwrap();
        assert!(
            (major, minor) < (0, 41),
            "datalib is {}: delete legacy_qmd_dir.rs and its two callers",
            crate::build_id::DATALIB_VERSION
        );
    }
}
