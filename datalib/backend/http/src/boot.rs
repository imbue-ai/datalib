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

use datalib_core::app_store::AppStore;
use datalib_core::repo::DynAppRepo;
use datalib_unified_index::repo::DynIndexRepo;
use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::qmd::{QmdDaemon, QmdDaemonConfig};

use crate::{auth::ApiToken, worker, AppState};

/// Open the data root (creating it if absent) and assemble the served
/// [`AppState`]: the doltlite repo at
/// [`datalib_core::layout::grid_index_db`], the lazy qmd daemon,
/// `<root>/config.toml`, the sync-progress channel, and the background
/// sync worker. The worker is spawned onto the ambient tokio runtime,
/// so this must be called from within one. `dag_bin` is the
/// `datalib-dag` runner the worker shells out to (with `binary_dir`
/// passed through as `--binary-dir` when resolved); `None` makes
/// UI-triggered syncs fail fast with a clear message while reads and
/// search still work. Presentation concerns (browser opening, the
/// `--url-file` handshake) live in the binary's main, not here.
///
/// `api_token` is minted by the caller rather than here because the
/// launch URL — announced through `--url-file` before this (slow)
/// assembly runs — has to carry it. We publish it to the data root
/// once the root exists, which is the first thing below.
pub async fn build_state(
    root: PathBuf,
    dag_bin: Option<PathBuf>,
    binary_dir: Option<PathBuf>,
    api_token: ApiToken,
) -> anyhow::Result<AppState> {
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    }
    let root = Arc::new(root);

    // Publish the token now that the root is on disk: anything running
    // as this user (an agent the UI handed a wayfinder to, a curl in a
    // terminal) reads it from here instead of scraping our stderr.
    api_token.write_token_file()?;

    eprintln!(
        "stores: {} (index, read-only here), {}, {}",
        datalib_core::layout::grid_index_db(&root).display(),
        datalib_core::layout::feedback_db(&root).display(),
        datalib_core::layout::jobs_db(&root).display(),
    );
    let repo: DynIndexRepo = Arc::new(
        DoltRepo::open(root.clone())
            .await
            .map_err(|e| anyhow::anyhow!("open the grid index under {}: {e}", root.display()))?,
    );
    let app: DynAppRepo = Arc::new(
        AppStore::open(&root)
            .await
            .map_err(|e| anyhow::anyhow!("open the app stores under {}: {e}", root.display()))?,
    );

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
    let worker_repo = app.clone();
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
        app,
        qmd_daemon,
        progress_tx,
        applets,
        api_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the web/Tauri drift this module exists to
    /// prevent: the state must open the doltlite file at the layout
    /// helper's path (`unified_index/grid/db.doltlite_db`), not some
    /// packaging-local filename at the root.
    #[tokio::test]
    async fn build_state_opens_the_layout_db_path() {
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("boot-test-token", root.path());
        let state = build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();
        let db_path = datalib_core::layout::grid_index_db(root.path());
        assert!(
            db_path.is_file(),
            "expected {} to be created",
            db_path.display()
        );
        assert_eq!(state.root.as_path(), root.path());
    }

    /// Three stores, three files — and the feedback store is not inside
    /// the index tree.
    ///
    /// Both halves are load-bearing. Sharing one file meant two
    /// processes writing it (this server and the `grid_index` step),
    /// and doltlite's working set is per file, so each one's
    /// `dolt_commit('-Am', …)` swept the other's in-flight rows. And the
    /// step tags its own tree with `CACHEDIR.TAG`, so feedback stored
    /// under it was feedback a `--exclude-caches` backup would skip.
    #[tokio::test]
    async fn build_state_keeps_the_three_stores_apart() {
        use datalib_core::layout;
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("split-test-token", root.path());
        build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();

        let index = layout::grid_index_db(root.path());
        let feedback = layout::feedback_db(root.path());
        let jobs = layout::jobs_db(root.path());
        for p in [&index, &feedback, &jobs] {
            assert!(p.is_file(), "expected a store at {}", p.display());
        }
        assert_ne!(index, feedback);
        assert_ne!(index, jobs);
        assert_ne!(feedback, jobs);

        // The tree the pipeline marks as rebuildable cache holds the
        // index and nothing else.
        let derived = layout::unified_index_dir(root.path());
        assert!(index.starts_with(&derived), "{}", index.display());
        assert!(
            !feedback.starts_with(&derived),
            "feedback must live outside the cache-tagged tree, got {}",
            feedback.display()
        );
        assert!(!jobs.starts_with(&derived), "{}", jobs.display());
    }

    /// The token has to reach disk during boot — it is how an agent
    /// (or a curl in a terminal) authenticates without scraping the
    /// server's stderr. A silently-missing file would look like a
    /// permissions problem much later, at the first 401.
    #[tokio::test]
    async fn build_state_publishes_the_api_token() {
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("published-token", root.path());
        let state = build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();
        let path = state.api_token.token_file();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "published-token",
            "expected the token at {}",
            path.display()
        );
    }
}
