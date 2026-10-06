//! `system/library-summary.json`: how many sources a library has, how
//! much it weighs and when it last synced, for the desktop app's list
//! of libraries. The app's shell links none of the stores these come
//! from, so the server that does writes them down each time it answers
//! the manage rows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ManageRow, RowKind};
use crate::DagRunInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySummary {
    pub sources: usize,
    pub bytes: u64,
    /// When the last finished run ended. Kept from the file while a run
    /// is going, since an unfinished run has no end yet.
    pub last_synced_at: Option<String>,
}

pub fn summary_path(root: &Path) -> PathBuf {
    datalib_core::layout::system_dir(root).join("library-summary.json")
}

pub fn summarize(
    rows: &[ManageRow],
    run: Option<&DagRunInfo>,
    bytes: u64,
    previous: Option<&LibrarySummary>,
) -> LibrarySummary {
    let sources = rows
        .iter()
        .filter(|r| r.kind == RowKind::Group && r.r#type.is_some() && r.dropped.is_none())
        .count();
    let last_synced_at = run
        .and_then(|r| r.finished_at.clone())
        .or_else(|| previous.and_then(|p| p.last_synced_at.clone()));
    LibrarySummary {
        sources,
        bytes,
        last_synced_at,
    }
}

/// Rewrite the file when the summary changed. Best-effort: a failure
/// costs the library list a figure, never the manage rows.
pub fn record(root: &Path, rows: &[ManageRow], run: Option<&DagRunInfo>, bytes: u64) {
    let path = summary_path(root);
    let previous: Option<LibrarySummary> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let next = summarize(rows, run, bytes, previous.as_ref());
    if previous.as_ref() == Some(&next) {
        return;
    }
    let written = serde_json::to_vec_pretty(&next)
        .map_err(std::io::Error::other)
        .and_then(|bytes| datalib_runtime::atomic::write(&path, &bytes));
    if let Err(e) = written {
        tracing::warn!("could not write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(finished_at: Option<&str>) -> DagRunInfo {
        DagRunInfo {
            run_id: "r".into(),
            started_at: "2026-10-01T10:00:00Z".into(),
            finished_at: finished_at.map(String::from),
            live: finished_at.is_none(),
        }
    }

    /// A run in flight has no end; the library list keeps saying when
    /// the one before it finished rather than "never".
    #[test]
    fn a_live_run_keeps_the_last_finish() {
        let before = LibrarySummary {
            sources: 1,
            bytes: 10,
            last_synced_at: Some("2026-09-30T08:00:00Z".into()),
        };
        let got = summarize(&[], Some(&run(None)), 20, Some(&before));
        assert_eq!(got.last_synced_at.as_deref(), Some("2026-09-30T08:00:00Z"));

        let got = summarize(
            &[],
            Some(&run(Some("2026-10-01T11:00:00Z"))),
            20,
            Some(&before),
        );
        assert_eq!(got.last_synced_at.as_deref(), Some("2026-10-01T11:00:00Z"));
    }

    #[test]
    fn a_library_that_never_ran_has_no_last_sync() {
        let got = summarize(&[], None, 0, None);
        assert_eq!(
            got,
            LibrarySummary {
                sources: 0,
                bytes: 0,
                last_synced_at: None
            }
        );
    }
}
