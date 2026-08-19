//! Backend assembly: everything derived from a data root — which
//! doltlite file to open, where the config lives, the qmd daemon, the
//! sync worker — in one place, so every packaging boots identically.
//! The `datalib-http` binary calls this directly; the Tauri shell
//! runs that same binary as a child process, so this is the single
//! boot path for both front doors. (History: the Tauri shell used to
//! link the backend in-process and duplicate this setup, kept the
//! pre-`system/` DB path when the layout moved, and silently served an
//! empty grid from a fresh, dataless DB.)

use std::path::PathBuf;
use std::sync::Arc;

use datalib_core::dolt_repo::DoltRepo;
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_core::repo::DynRepo;

use crate::{worker, AppState};

/// Open the data root (creating it if absent) and assemble the served
/// [`AppState`]: the doltlite repo at
/// [`datalib_core::layout::backend_index_db`], the lazy qmd daemon,
/// `<root>/config.toml`, the sync-progress channel, and the background
/// sync worker. The worker is spawned onto the ambient tokio runtime,
/// so this must be called from within one. `dag_bin` is the
/// `datalib-dag` runner the worker shells out to (with `binary_dir`
/// passed through as `--binary-dir` when resolved); `None` makes
/// UI-triggered syncs fail fast with a clear message while reads and
/// search still work. Presentation concerns (browser opening, the
/// `--url-file` handshake) live in the binary's main, not here.
pub async fn build_state(
    root: PathBuf,
    dag_bin: Option<PathBuf>,
    binary_dir: Option<PathBuf>,
) -> anyhow::Result<AppState> {
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    }
    let root = Arc::new(root);

    let db_path = datalib_core::layout::backend_index_db(&root);
    eprintln!("dolt db: {}", db_path.display());
    let repo = DoltRepo::open(&db_path, root.clone())
        .await
        .map_err(|e| anyhow::anyhow!("open doltlite at {}: {e}", db_path.display()))?;
    let repo: DynRepo = Arc::new(repo);

    // The daemon resolves its index lazily per search, so an empty root
    // (no sync yet) or a mid-session rebuild is handled transparently —
    // search falls back until the index exists, then upgrades to qmd
    // with no restart. Models are lazy too: the indexer warms the
    // shared cache during sync, and a cold cache pays a one-time
    // download on the first semantic search instead of blocking boot.
    let qmd_daemon = Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone())));

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
        dag_bin,
        binary_dir: binary_dir.clone(),
        progress_tx: progress_tx.clone(),
    };
    let worker_repo = repo.clone();
    tokio::spawn(async move {
        worker::run(worker_repo, worker_cfg).await;
    });

    // Applet discovery execs one child per configured applet, and
    // `build_state` runs on the tokio runtime — so it goes to a
    // blocking thread rather than stalling the executor while a slow
    // binary starts. Config policy lives in `AppletRegistry`.
    let data_root = (*root).clone();
    let applets = tokio::task::spawn_blocking(move || {
        Arc::new(crate::applets::AppletRegistry::from_data_root(
            &data_root, binary_dir,
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("applet discovery panicked: {e}"))?;

    Ok(AppState {
        root,
        repo,
        qmd_daemon,
        progress_tx,
        applets,
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
        let state = build_state(root.path().to_path_buf(), None, None)
            .await
            .unwrap();
        let db_path = datalib_core::layout::backend_index_db(root.path());
        assert!(
            db_path.is_file(),
            "expected {} to be created",
            db_path.display()
        );
        assert_eq!(state.root.as_path(), root.path());
    }
}
