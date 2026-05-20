//! Frankweiler HTTP server entrypoint.
//!
//! Resolves the data root in this order:
//!   1. $FRANKWEILER_ROOT
//!   2. `data_root:` from $FRANKWEILER_CONFIG or ~/.config/frankweiler/config.yaml
//!   3. ~/Documents/mixed-up-files (default)
//!
//! Bind address is $FRANKWEILER_BIND if set, else backend.bind from the
//! config file, else 127.0.0.1:8731. The env override exists primarily for
//! tests that need a non-default port without writing a config file.
//!
//! Backend: [`DoltRepo`](frankweiler_core::dolt_repo::DoltRepo). We spawn
//! (or attach to) a managed `dolt sql-server` at
//! `<root>/<dolt.repo_dirname>` and connect a `sqlx::MySqlPool`.
//!
//! The server starts even if the root or Dolt repo doesn't exist yet —
//! `/api/search` will just return zero rows. `/api/health` reports the
//! resolved root and whether it exists, which is handy when wiring up the UI.

use frankweiler_core::config::{
    default_config_path, load_config, BackendConfig, Config, ConfigError,
};
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_core::qmd::{QmdDaemon, QmdDaemonConfig};
use frankweiler_core::repo::DynRepo;
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config: missing default config path is fine (we fall back to env +
    // defaults), but any other error (parse, IO, validation) is fatal —
    // silently swallowing them masks typos in $FRANKWEILER_CONFIG.
    let cfg_path = default_config_path();
    let explicit_cfg = std::env::var("FRANKWEILER_CONFIG").is_ok();
    let cfg_opt = match load_config(Some(&cfg_path)) {
        Ok(c) => Some(c),
        Err(ConfigError::NotFound(p)) if !explicit_cfg => {
            eprintln!("config: no file at {} (using defaults)", p.display());
            None
        }
        Err(e) => return Err(anyhow::anyhow!("config {}: {e}", cfg_path.display())),
    };
    let (bind, root) = resolve_bind_and_root(cfg_opt.as_ref());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "frankweiler-http listening on http://{}",
        listener.local_addr()?
    );
    // Auto-create the data root so a fresh install (or a typo'd dev config
    // pointing at a not-yet-created dir) doesn't require manual mkdir.
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("create data_root {}: {e}", root.display()))?;
        eprintln!("data root: {} (created)", root.display());
    } else {
        eprintln!("data root: {}", root.display());
    }

    let root = Arc::new(root);
    let (repo, dolt_server) = build_repo(cfg_opt.as_ref(), root.clone()).await?;
    // Best-effort daemon spawn. If the index isn't materialized yet
    // `QmdDaemon::new` fails fast; the search path then falls back to
    // the per-call CLI shell-out, so /api/search still works.
    let qmd_daemon = match QmdDaemon::new(QmdDaemonConfig::new((*root).clone())) {
        Ok(d) => {
            eprintln!("qmd daemon: ready (lazy spawn on first search)");
            Some(Arc::new(d))
        }
        Err(e) => {
            eprintln!("qmd daemon: disabled ({e:#}); falling back to CLI per call");
            None
        }
    };
    let state = AppState {
        root,
        repo,
        dolt_server,
        qmd_daemon,
    };
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn build_repo(
    cfg: Option<&Config>,
    root: Arc<PathBuf>,
) -> anyhow::Result<(DynRepo, Option<Arc<DoltServer>>)> {
    let dolt_cfg = cfg.map(|c| c.dolt.clone()).unwrap_or_default();
    let repo_dir = match cfg {
        Some(c) => c.dolt_db_path(),
        None => root.as_ref().join(&dolt_cfg.repo_dirname),
    };
    eprintln!(
        "dolt repo: {} (host={} port={})",
        repo_dir.display(),
        dolt_cfg.host,
        dolt_cfg.port
    );
    let server = DoltServer::ensure(&repo_dir, &dolt_cfg)
        .map_err(|e| anyhow::anyhow!("dolt sql-server: {e}"))?;
    let url = server.mysql_url();
    let repo = DoltRepo::connect(&url, root)
        .await
        .map_err(|e| anyhow::anyhow!("connect dolt mysql at {url}: {e}"))?;
    Ok((Arc::new(repo), Some(Arc::new(server))))
}

fn resolve_bind_and_root(cfg: Option<&Config>) -> (String, PathBuf) {
    let bind = std::env::var("FRANKWEILER_BIND").ok().unwrap_or_else(|| {
        cfg.map(|c| c.backend.bind.clone())
            .unwrap_or_else(|| BackendConfig::default().bind)
    });
    let root = if let Ok(env) = std::env::var("FRANKWEILER_ROOT") {
        expand_tilde(&env)
    } else if let Some(c) = cfg {
        c.data_root.clone()
    } else {
        default_root()
    };
    (bind, root)
}

fn default_root() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Documents/mixed-up-files");
    }
    PathBuf::from("./mixed-up-files")
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}
