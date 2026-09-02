//! Backend assembly: everything derived from a data root — the stores
//! this server owns, where the config lives, the sync worker, the disk
//! usage sampler — in one place, so every packaging boots identically.
//!
//! The grid and qmd indexes are not here. They belong to the
//! `unified_index` applet, which the gateway spawns from `config.toml`
//! like any other; this process never opens them.
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

use crate::{auth::ApiToken, usage, worker, AppState};

/// Open the data root (creating it if absent) and assemble the served
/// [`AppState`]: the feedback and job stores, `<root>/config.toml`, the
/// sync-progress channel, and the background sync worker. The worker is spawned onto the ambient tokio runtime,
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
        "stores: {}, {}",
        datalib_core::layout::feedback_db(&root).display(),
        datalib_core::layout::jobs_db(&root).display(),
    );
    let app: DynAppRepo = Arc::new(
        AppStore::open(&root)
            .await
            .map_err(|e| anyhow::anyhow!("open the app stores under {}: {e}", root.display()))?,
    );

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

    // Bytes on disk, over time: one walk of the root per tick, folded
    // into a snapshot the storage endpoint reads and appended to
    // `system/usage.doltlite_db`. Spawned rather than awaited — the
    // first walk of a large root is slow, and a boot that waited for it
    // would delay the whole server for a number nothing needs yet.
    let monitor = Arc::new(usage::UsageMonitor::new());
    tokio::spawn(usage::run(monitor.clone(), app.clone(), root.clone()));

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
        app,
        progress_tx,
        applets,
        api_token,
        usage: monitor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the web/Tauri drift this module exists to
    /// prevent: the state must open its stores at the layout helpers'
    /// paths, not some packaging-local filename at the root.
    #[tokio::test]
    async fn build_state_opens_the_layout_store_paths() {
        use datalib_core::layout;
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("boot-test-token", root.path());
        let state = build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();
        for p in [
            layout::feedback_db(root.path()),
            layout::jobs_db(root.path()),
        ] {
            assert!(p.is_file(), "expected {} to be created", p.display());
        }
        assert_eq!(state.root.as_path(), root.path());
    }

    /// The server does not touch the search indexes.
    ///
    /// This is the whole point of the `unified_index` applet: booting
    /// the server must not open — or create — the grid index, because
    /// the applet owns it and the pipeline writes it. A regression here
    /// would be invisible in behaviour (the file would just exist
    /// again) and would quietly restore the two-writer arrangement the
    /// store split removed.
    #[tokio::test]
    async fn build_state_never_touches_the_index() {
        use datalib_core::layout;
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("no-index-token", root.path());
        build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();
        assert!(
            !layout::unified_index_dir(root.path()).exists(),
            "booting the server created {}, which belongs to the applet",
            layout::unified_index_dir(root.path()).display()
        );
    }

    /// The two stores this server does own are separate files, and
    /// neither is inside the tree the pipeline tags as rebuildable
    /// cache — feedback is not regenerable and must survive a
    /// `--exclude-caches` backup.
    #[tokio::test]
    async fn build_state_keeps_the_stores_apart() {
        use datalib_core::layout;
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("split-test-token", root.path());
        build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();

        let feedback = layout::feedback_db(root.path());
        let jobs = layout::jobs_db(root.path());
        assert_ne!(feedback, jobs);
        let derived = layout::unified_index_dir(root.path());
        assert!(!feedback.starts_with(&derived), "{}", feedback.display());
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
