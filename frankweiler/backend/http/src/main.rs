//! Frankweiler HTTP server entrypoint.
//!
//! Resolves the data root in this order:
//!   1. $FRANKWEILER_ROOT
//!   2. `root:` from $FRANKWEILER_CONFIG or ~/.config/frankweiler/config.yaml
//!   3. ~/Documents/personal-mirror (default)
//!
//! Bind address is $FRANKWEILER_BIND if set, else backend.bind from the
//! config file, else 127.0.0.1:8731. The env override exists primarily for
//! tests that need a non-default port without writing a config file.
//!
//! Backend selection:
//!   * Default: production [`DoltRepo`](frankweiler_core::dolt_repo::DoltRepo).
//!     We spawn (or attach to) a managed `dolt sql-server` at
//!     `<root>/<dolt.repo_dirname>` and connect a `sqlx::MySqlPool`.
//!   * `--backend sqlite`: read-only [`SqliteRepo`](frankweiler_core::sqlite_repo::SqliteRepo)
//!     against `<root>/mirror.sqlite`. Reference / debug path; falls back
//!     to an empty in-memory DB if the file is missing so the server still
//!     starts.
//!
//! The server starts even if the root or Dolt repo doesn't exist yet —
//! `/api/search` will just return zero rows. `/api/health` reports the
//! resolved root and whether it exists, which is handy when wiring up the UI.

use frankweiler_core::config::{default_config_path, load_config, BackendConfig, Config};
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_core::repo::DynRepo;
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Dolt,
    Sqlite,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend_kind = parse_backend_flag();
    let cfg_opt = load_config(Some(&default_config_path())).ok();
    let (bind, root) = resolve_bind_and_root(cfg_opt.as_ref());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "frankweiler-http listening on http://{}",
        listener.local_addr()?
    );
    eprintln!("data root: {} (exists={})", root.display(), root.exists());
    eprintln!("backend: {:?}", backend_kind);

    let root = Arc::new(root);
    let (repo, dolt_server) = build_repo(backend_kind, cfg_opt.as_ref(), root.clone()).await?;
    let state = AppState {
        root,
        repo,
        dolt_server,
    };
    axum::serve(listener, router(state)).await?;
    Ok(())
}

fn parse_backend_flag() -> BackendKind {
    // Tiny hand-rolled flag parser — only one knob, no need for clap.
    // Accepts `--backend sqlite|dolt` and `--backend=sqlite|dolt`.
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--backend=") {
            return parse_backend_value(v);
        }
        if a == "--backend" && i + 1 < args.len() {
            return parse_backend_value(&args[i + 1]);
        }
        i += 1;
    }
    BackendKind::Dolt
}

fn parse_backend_value(s: &str) -> BackendKind {
    match s.to_ascii_lowercase().as_str() {
        "sqlite" => BackendKind::Sqlite,
        "dolt" => BackendKind::Dolt,
        other => {
            eprintln!("unknown --backend value {other:?}, defaulting to dolt");
            BackendKind::Dolt
        }
    }
}

async fn build_repo(
    kind: BackendKind,
    cfg: Option<&Config>,
    root: Arc<PathBuf>,
) -> anyhow::Result<(DynRepo, Option<Arc<DoltServer>>)> {
    match kind {
        BackendKind::Sqlite => {
            // Use the existing default factory — falls back to an empty
            // in-memory DB if `mirror.sqlite` is missing.
            let repo = frankweiler_core::repo::default_repo(root).await;
            Ok((repo, None))
        }
        BackendKind::Dolt => {
            let dolt_cfg = cfg.map(|c| c.dolt.clone()).unwrap_or_default();
            let repo_dir = match cfg {
                Some(c) => c.dolt_repo_path(),
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
    }
}

fn resolve_bind_and_root(cfg: Option<&Config>) -> (String, PathBuf) {
    let bind = std::env::var("FRANKWEILER_BIND").ok().unwrap_or_else(|| {
        cfg.map(|c| c.backend.bind.clone())
            .unwrap_or_else(|| BackendConfig::default().bind)
    });
    let root = if let Ok(env) = std::env::var("FRANKWEILER_ROOT") {
        expand_tilde(&env)
    } else if let Some(c) = cfg {
        c.root.clone()
    } else {
        default_root()
    };
    (bind, root)
}

fn default_root() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Documents/personal-mirror");
    }
    PathBuf::from("./personal-mirror")
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}
