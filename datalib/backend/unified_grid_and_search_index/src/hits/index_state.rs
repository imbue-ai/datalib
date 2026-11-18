//! What `/index_state` answers about the search index, and how an index
//! file is seen to move.

use std::path::Path;

use serde::Serialize;

/// Index-wide totals, for the "N of M documents searchable" line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IndexSummary {
    /// Documents the search index holds.
    pub documents: u64,
    /// …of which have a vector for every chunk.
    pub embedded: u64,
}

/// What an index file looks like on disk: the size and modification time
/// of the file and of its write-ahead log. A writer in WAL mode grows the
/// log at a commit and rewrites the main file at a checkpoint; either
/// moves this. A missing file reads as `None`, and so does
/// an empty log: it holds nothing, and a reader that opens the file makes
/// one, which would otherwise read as the index moving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStamp([Option<(u64, std::time::SystemTime)>; 2]);

impl IndexStamp {
    pub fn of(index: &Path) -> Self {
        let stamp = |p: &Path| {
            let m = std::fs::metadata(p).ok()?;
            Some((m.len(), m.modified().ok()?))
        };
        let mut wal = index.as_os_str().to_owned();
        wal.push("-wal");
        let log = stamp(Path::new(&wal)).filter(|(len, _)| *len > 0);
        Self([stamp(index), log])
    }

    /// When the index or its write-ahead log last changed; `None` when
    /// neither file is there.
    pub fn last_change(&self) -> Option<std::time::SystemTime> {
        self.0.iter().flatten().map(|(_, at)| *at).max()
    }
}

/// One markdown's index state, as reported to a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocReport {
    pub indexed: Option<bool>,
    pub embedded: Option<bool>,
    /// Why the answer is unknown. `None` on the ordinary paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
