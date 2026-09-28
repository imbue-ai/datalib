//! A config in the shape from before each source had its own qmd steps is
//! rewritten in place as soon as the server sees it: at boot, and whenever
//! the file changes. The text it replaced is kept as `config.toml.bak`.
//! The rewrite itself is `datalib_migrate_config::upgrade_qmd_steps`.

use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::watch::{RootEvent, RootFrame};

/// Rewrite the root's config if it is in the old shape, and say so in the
/// log. Returns where the old text went, when it did.
pub fn upgrade(root: &Path) -> Option<PathBuf> {
    let path = datalib_dag::config::root_config_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    let upgraded = match datalib_migrate_config::upgrade_qmd_steps(&text) {
        Ok(Some(upgraded)) => upgraded,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(
                "config: {} is in the shape from before each source had its own qmd \
                 steps, and was left as it is: {e:#}. `datalib-migrate-config {} --force` \
                 shows the rewrite.",
                path.display(),
                root.display()
            );
            return None;
        }
    };
    let bak = path.with_extension("toml.bak");
    let written = crate::write_owner_only(&bak, text.as_bytes())
        .and_then(|()| crate::replace_config(&path, &upgraded));
    if let Err(e) = written {
        tracing::error!("config: could not migrate {}: {e}", path.display());
        return None;
    }
    tracing::warn!(
        "config: migrated {}: each source the qmd index names now has its own \
         keyword_index and embed steps. The previous file is {}.",
        path.display(),
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
