//! A config in a shape the server can bring forward by itself — a
//! `qmd_index` step from before `qmd_aggregator`, a retired
//! `always_clear_before_ingest` — is rewritten in place as soon as the
//! server sees it: at boot, and whenever the file changes. The text it
//! replaced is kept as `config.toml.bak`. The rewrites themselves are
//! `datalib_migrate_config::upgrade`.

use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::watch::{RootEvent, RootFrame};

/// Rewrite the root's config if it is in an older shape, and say so in the
/// log. Returns where the old text went, when it did.
pub fn upgrade(root: &Path) -> Option<PathBuf> {
    let path = datalib_dag::config::root_config_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    let upgraded = match datalib_migrate_config::upgrade(&text) {
        Ok(Some(upgraded)) => upgraded,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(
                "config: {} is in an older shape, and was left as it is: {e:#}. \
                 `datalib-migrate-config {} --force` shows the rewrite.",
                path.display(),
                root.display()
            );
            return None;
        }
    };
    let bak = path.with_extension("toml.bak");
    let written = datalib_dag::config::write_owner_only(&bak, text.as_bytes())
        .and_then(|()| datalib_dag::config::replace_config(&path, &upgraded.text));
    if let Err(e) = written {
        tracing::error!("config: could not migrate {}: {e}", path.display());
        return None;
    }
    let made: Vec<&str> = upgraded.made.iter().map(|u| u.describe()).collect();
    tracing::warn!(
        "config: migrated {}: {}. The previous file is {}.",
        path.display(),
        made.join("; "),
        bak.display()
    );
    Some(bak)
}

/// Upgrade on every change the root watcher reports, a hand edit or a save
/// alike. A rewrite is itself a change, and the next pass finds nothing to do.
pub fn watch(root: PathBuf, mut rx: broadcast::Receiver<RootFrame>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(RootFrame {
                    event: RootEvent::ConfigChanged,
                    ..
                })
                | Err(broadcast::error::RecvError::Lagged(_)) => {
                    let root = root.clone();
                    let _ = tokio::task::spawn_blocking(move || upgrade(&root)).await;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}
