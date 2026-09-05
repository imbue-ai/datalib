//! The on-disk shape of a per-source raw store directory.

use std::path::{Path, PathBuf};

/// Entity tables + per-provider CAS edge tables + shared sync
/// bookkeeping. The primary doltlite database for a source.
pub const ENTITIES_DB: &str = "entities.doltlite_db";

/// Content-addressed blob store: a single `cas_objects` table keyed by
/// blake3 hash. Sibling of [`ENTITIES_DB`] inside the same raw dir.
pub const BLOBS_DB: &str = "blobs.doltlite_db";

/// Plain-text, append-only JSONL mirror of what came off the wire, one
/// subfile per table. Debug aid only — never read by the pipeline, safe
/// to delete. See `docs/dev/data_architecture_ingestion.md`.
pub const EVENTS_DIR: &str = "events";

pub fn entities_db(raw_dir: &Path) -> PathBuf {
    raw_dir.join(ENTITIES_DB)
}

pub fn blobs_db(raw_dir: &Path) -> PathBuf {
    raw_dir.join(BLOBS_DB)
}

pub fn events_dir(raw_dir: &Path) -> PathBuf {
    raw_dir.join(EVENTS_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_inside_the_raw_dir() {
        let dir = Path::new("/tmp/raw/slack");
        assert_eq!(
            entities_db(dir),
            PathBuf::from("/tmp/raw/slack/entities.doltlite_db")
        );
        assert_eq!(
            blobs_db(dir),
            PathBuf::from("/tmp/raw/slack/blobs.doltlite_db")
        );
        assert_eq!(events_dir(dir), PathBuf::from("/tmp/raw/slack/events"));
    }
}
