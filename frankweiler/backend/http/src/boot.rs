//! Shared backend assembly for the two front doors.
//!
//! The standalone `frankweiler-http` binary and the Tauri shell both
//! serve [`crate::router`] against a data root. Everything derived from
//! that root — which doltlite file to open, where the config lives, the
//! qmd daemon, the sync worker — is assembled here so the two
//! packagings cannot drift apart. (The Tauri shell used to duplicate
//! this setup and kept the pre-`system/` DB path when the layout moved,
//! silently serving an empty grid from a fresh, dataless DB.)
//! Divergences between the packagings should stay presentation-level:
//! how fatal errors surface (dialog vs stderr), qmd model prefetch,
//! opening a browser.

use std::path::PathBuf;
use std::sync::Arc;

use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::qmd::{QmdDaemon, QmdDaemonConfig};
use frankweiler_core::repo::DynRepo;

use crate::{worker, AppState};

/// Open the data root (creating it if absent) and assemble the
/// [`AppState`] both packagings serve: the doltlite repo at
/// [`frankweiler_core::layout::backend_index_db`], the lazy qmd daemon,
/// `<root>/config.yaml`, the sync-progress channel, and the background
/// sync worker. The worker is spawned onto the ambient tokio runtime,
/// so this must be called from within one (both callers are). `sync_bin`
/// is the `frankweiler-sync` binary the worker shells out to; `None`
/// makes UI-triggered syncs fail fast with a clear message while reads
/// and search still work.
pub async fn build_state(root: PathBuf, sync_bin: Option<PathBuf>) -> anyhow::Result<AppState> {
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    }
    let root = Arc::new(root);

    let db_path = frankweiler_core::layout::backend_index_db(&root);
    eprintln!("dolt db: {}", db_path.display());
    let repo = DoltRepo::open(&db_path, root.clone())
        .await
        .map_err(|e| anyhow::anyhow!("open doltlite at {}: {e}", db_path.display()))?;
    let repo: DynRepo = Arc::new(repo);

    // The daemon resolves its index lazily per search, so an empty root
    // (no sync yet) or a mid-session rebuild is handled transparently —
    // search falls back until the index exists, then upgrades to qmd
    // with no restart. Model prefetch is a packaging concern (the
    // standalone binary primes eagerly, the Tauri shell stays lazy) and
    // lives in the callers.
    let qmd_daemon = Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone())));

    // Self-contained config: the app reads/writes `<root>/config.yaml`,
    // so a fresh data root needs no external `~/.config` file.
    let config_path = Arc::new(frankweiler_ingest_config::root_config_path(&root));

    // Live sync-job progress fan-out: the worker + enqueue/cancel
    // handlers publish here, `GET /api/sync/stream` subscribes over SSE.
    // Buffer a few hundred events so a briefly-stalled client lags
    // rather than blocks the worker.
    let (progress_tx, _) = tokio::sync::broadcast::channel(512);

    // Background sync worker: drains the `sync_jobs` queue the UI fills.
    // With no sync binary it still runs — UI-triggered syncs fail fast
    // with a clear message instead of hanging (search is unaffected).
    let worker_cfg = worker::WorkerConfig {
        root: root.clone(),
        config_path: (*config_path).clone(),
        sync_bin,
        progress_tx: progress_tx.clone(),
    };
    let worker_repo = repo.clone();
    tokio::spawn(async move {
        worker::run(worker_repo, worker_cfg).await;
    });

    Ok(AppState {
        root,
        config_path,
        repo,
        qmd_daemon,
        progress_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the web/Tauri drift this module exists to
    /// prevent: the state must open the doltlite file at the layout
    /// helper's path (`system/backend_index/db.doltlite_db`), not some
    /// packaging-local filename at the root.
    #[tokio::test]
    async fn build_state_opens_the_layout_db_path() {
        let root = tempfile::tempdir().unwrap();
        let state = build_state(root.path().to_path_buf(), None).await.unwrap();
        let db_path = frankweiler_core::layout::backend_index_db(root.path());
        assert!(
            db_path.is_file(),
            "expected {} to be created",
            db_path.display()
        );
        assert_eq!(state.root.as_path(), root.path());
    }
}
