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
//! The server starts even if the root doesn't exist yet — `/api/search` will
//! just return zero rows. `/api/health` reports the resolved root and whether
//! it exists, which is handy when wiring up the UI.

use frankweiler_core::config::{default_config_path, load_config, BackendConfig};
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (bind, root) = resolve_bind_and_root();
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "frankweiler-http listening on http://{}",
        listener.local_addr()?
    );
    eprintln!("data root: {} (exists={})", root.display(), root.exists());
    let root = Arc::new(root);
    let repo = frankweiler_core::repo::default_repo(root.clone());
    let state = AppState { root, repo };
    axum::serve(listener, router(state)).await?;
    Ok(())
}

fn resolve_bind_and_root() -> (String, PathBuf) {
    let cfg = load_config(Some(&default_config_path())).ok();
    let bind = std::env::var("FRANKWEILER_BIND").ok().unwrap_or_else(|| {
        cfg.as_ref()
            .map(|c| c.backend.bind.clone())
            .unwrap_or_else(|| BackendConfig::default().bind)
    });
    let root = if let Ok(env) = std::env::var("FRANKWEILER_ROOT") {
        expand_tilde(&env)
    } else if let Some(c) = cfg {
        c.root
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
