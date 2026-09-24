//! Backend assembly: everything derived from a data root — the stores
//! this server owns, where the config lives, the supervisor loop, the
//! disk usage sampler — in one place, so every packaging boots
//! identically.

use std::path::PathBuf;
use std::sync::Arc;

use datalib_core::app_store::AppStore;
use datalib_core::repo::DynAppRepo;

use crate::{auth::ApiToken, supervisor, usage, AppState};

/// Open the data root (creating it if absent) and assemble the served
/// [`AppState`]: the stores it owns, `<root>/config.toml`, and the
/// supervisor loop, spawned onto the
/// ambient tokio runtime — so this must be called from within one.
/// `binary_dir` goes first on every step's `PATH`, ahead of the config's
/// own `binary_dir`; `now`, when given, stands in for the clock in every
/// sync the loop runs (`--now`). Presentation concerns (browser opening, the
/// `--url-file` handshake) live in the binary's main, not here.
pub async fn build_state(
    root: PathBuf,
    binary_dir: Option<PathBuf>,
    now: Option<String>,
    api_token: ApiToken,
) -> anyhow::Result<AppState> {
    datalib_core::layout::create_data_root(&root)
        .map_err(|e| anyhow::anyhow!("create data root {}: {e}", root.display()))?;
    let root = Arc::new(root);

    // Publish the token now that the root is on disk: anything running
    // as this user (an agent the UI handed a wayfinder to, a curl in a
    // terminal) reads it from here instead of scraping our stderr.
    api_token.write_token_file()?;

    // A root a newer line of datalib wrote is not opened at all: this
    // server then boots only to show the gate that says so, with a repo
    // that refuses every call, no loop and no sampler. Checked before
    // the app stores because opening them is the first write.
    let newer_root = datalib_store_meta::inspect_root(&root).await;
    if !newer_root.is_empty() {
        for n in &newer_root {
            tracing::error!("{n}");
        }
        let (root_tx, _) = tokio::sync::broadcast::channel(64);
        crate::watch::spawn((*root).clone(), root_tx.clone());
        return Ok(AppState {
            root: root.clone(),
            app: Arc::new(NewerRootRepo(newer_root.clone())),
            sync: supervisor::SyncControl::new(root.clone()),
            root_tx,
            // No entries: nothing starts, and the unified_index applet
            // never opens the index this build must not read as its own.
            applets: Arc::new(crate::applets::AppletRegistry::build(
                Vec::new(),
                (*root).clone(),
                None,
            )),
            api_token,
            usage: Arc::new(usage::UsageMonitor::new()),
            newer_root,
        });
    }

    tracing::info!(
        "stores: {}",
        datalib_core::layout::feedback_db(&root).display(),
    );
    let app: DynAppRepo = Arc::new(
        AppStore::open(&root)
            .await
            .map_err(|e| anyhow::anyhow!("open the app stores under {}: {e}", root.display()))?,
    );

    // Everything the UI shows that changes. One watcher per
    // process, replacing the timers every UI surface used to keep — see
    // `crate::watch`. Started before the loop so a sync that begins
    // during startup is already being reported on.
    let (root_tx, _) = tokio::sync::broadcast::channel(64);
    crate::watch::spawn((*root).clone(), root_tx.clone());

    let sync = supervisor::SyncControl::new(root.clone());
    tokio::spawn(supervisor::run(supervisor::HostConfig {
        control: sync.clone(),
        binary_dir: binary_dir.clone(),
        now,
    }));

    // Bytes on disk, over time: a walk of the root folded into a
    // snapshot the storage endpoint reads and appended to
    // `system/usage.doltlite_db`. Driven by `root_tx` rather than a
    // timer of its own — a run in flight is exactly what that channel
    // is already reporting, and between runs the disk cannot have
    // moved. Spawned rather than awaited: the first walk of a large
    // root is slow, and a boot that waited for it would delay the whole
    // server for a number nothing needs yet.
    let monitor = Arc::new(usage::UsageMonitor::new());
    tokio::spawn(usage::run(
        monitor.clone(),
        app.clone(),
        root.clone(),
        root_tx.clone(),
        sync.clone(),
    ));

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
    // Hand edits reach the registry from here; saves reach it from the
    // handlers that make them. Requests only read it.
    crate::applets::watch_config(applets.clone(), root_tx.subscribe());

    Ok(AppState {
        root,
        app,
        sync,
        root_tx,
        applets,
        api_token,
        usage: monitor,
        newer_root: Vec::new(),
    })
}

/// The repo a refused root gets: every call fails with the refusal, so
/// a caller that reaches past the gate — a curl, an agent — is told why
/// rather than handed an empty answer.
struct NewerRootRepo(Vec<datalib_store_meta::NewerBuild>);

impl NewerRootRepo {
    fn refuse<T>(&self) -> Result<T, datalib_core::repo::RepoError> {
        Err(datalib_core::repo::RepoError::Internal(
            self.0
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[async_trait::async_trait]
impl datalib_core::repo::AppRepo for NewerRootRepo {
    async fn insert_feedback(
        &self,
        _row: app_schema::feedback::FeedbackRow,
    ) -> Result<(), datalib_core::repo::RepoError> {
        self.refuse()
    }
    async fn recent_disk_usage(
        &self,
        _limit: usize,
    ) -> Result<Vec<app_schema::disk_usage::DiskUsageRow>, datalib_core::repo::RepoError> {
        self.refuse()
    }
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
        let p = layout::feedback_db(root.path());
        assert!(p.is_file(), "expected {} to be created", p.display());
        assert_eq!(state.root.as_path(), root.path());
        assert!(state.newer_root.is_empty());
    }

    /// A root a newer datalib wrote still boots — to say so. The app
    /// stores are not opened (their meta is untouched), the repo refuses
    /// with the reason, and `GET /api/config` reports `app_ready: false`
    /// with the stores and both versions for the screen.
    #[tokio::test]
    async fn a_root_a_newer_build_wrote_boots_only_to_say_so() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("boot-test-token", root.path());
        // A first boot writes the stores; a "newer release" then marks one.
        drop(
            build_state(root.path().to_path_buf(), None, None, token.clone())
                .await
                .unwrap(),
        );
        let feedback = datalib_core::layout::feedback_db(root.path());
        {
            let pool = datalib_core::store::open_pool(&feedback).await.unwrap();
            sqlx::query("UPDATE _datalib_meta SET value = '99.0.0' WHERE key = 'datalib_version'")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let state = build_state(root.path().to_path_buf(), None, None, token.clone())
            .await
            .expect("boots to show the gate");
        assert_eq!(state.newer_root.len(), 1);
        assert_eq!(state.newer_root[0].wrote, "99.0.0");
        let err = state
            .app
            .recent_disk_usage(1)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("written by datalib 99.0.0"), "{err}");
        // Not opened: still says what the newer release wrote.
        let meta = datalib_store_meta::guard::read_at(&feedback)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.datalib_version, "99.0.0");

        let app = crate::router(state);
        let resp = app
            .oneshot(
                Request::get("/api/config")
                    .header("authorization", format!("Bearer {}", token.value()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["app_ready"], false);
        assert_eq!(v["newer_root"]["stores"][0]["wrote"], "99.0.0");
        assert_eq!(
            v["newer_root"]["stores"][0]["store"],
            "system/feedback.doltlite_db"
        );
        assert_eq!(
            v["newer_root"]["running"],
            datalib_runtime::build_id::DATALIB_VERSION
        );
    }

    /// The server does not touch the search indexes.
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

    /// Feedback is not inside the tree the pipeline tags as rebuildable
    /// cache: it is not regenerable and must survive a
    /// `--exclude-caches` backup.
    #[tokio::test]
    async fn build_state_keeps_feedback_out_of_the_cache() {
        use datalib_core::layout;
        let root = tempfile::tempdir().unwrap();
        let token = ApiToken::from_value("split-test-token", root.path());
        build_state(root.path().to_path_buf(), None, None, token)
            .await
            .unwrap();

        let feedback = layout::feedback_db(root.path());
        let derived = layout::unified_index_dir(root.path());
        assert!(!feedback.starts_with(&derived), "{}", feedback.display());
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
