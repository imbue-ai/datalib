//! A config in a shape the server can bring forward unattended is
//! rewritten in place as soon as the server sees it: at boot, on a save
//! through the API, and whenever the file changes. Three rewrites, in
//! order: a `qmd_index` step from
//! before `qmd_aggregator` (`datalib_migrate_config::upgrade_qmd_steps`),
//! the index group still named `unified_index`
//! (`datalib_migrate_config::upgrade_document_index`), and a config with
//! time-series sources and no time-series index
//! (`datalib_migrate_config::upgrade_time_series_steps`). The text they
//! replaced is kept as `config.toml.bak`.
//!
//! At boot only, [`move_index_dir`] then brings the document index's tree
//! and the loop's record of its steps over to the new name, before
//! anything opens them.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::watch::{RootEvent, RootFrame};

type Rewrite = fn(&str) -> anyhow::Result<Option<String>>;

/// Each unattended rewrite: what it is for, in the log, and what it does.
const REWRITES: [(&str, &str, Rewrite); 3] = [
    (
        "still has a `qmd_index` step",
        "`qmd_index` is now `qmd_aggregator`, reading each source's own \
         keyword_index and embed steps",
        datalib_migrate_config::upgrade_qmd_steps,
    ),
    (
        "still names its index group `unified_index`",
        "the `unified_index` group, its steps and its applet are now \
         `unified_document_index`",
        datalib_migrate_config::upgrade_document_index,
    ),
    (
        "has a source with time series and no time-series index",
        "each source with time series has a `render_time_series` step, read by \
         the new `unified_time_series_index` group's `time_series_index`",
        datalib_migrate_config::upgrade_time_series_steps,
    ),
];

/// A rewrite reads the file and writes it back, so a save landing between
/// the two would be lost: this process's writers of the config take turns.
static WRITING: Mutex<()> = Mutex::new(());

fn writing() -> MutexGuard<'static, ()> {
    WRITING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Write `text` as the root's config, already brought forward.
pub fn save(root: &Path, text: &str) -> std::io::Result<()> {
    let _turn = writing();
    let path = datalib_dag::config::root_config_path(root);
    datalib_runtime::atomic::write_owner_only(&path, text.as_bytes())?;
    upgrade_in_turn(root);
    Ok(())
}

/// Rewrite the root's config if it is in an old shape, and say so in the
/// log. Returns where the old text went, when it did.
pub fn upgrade(root: &Path) -> Option<PathBuf> {
    let _turn = writing();
    upgrade_in_turn(root)
}

fn upgrade_in_turn(root: &Path) -> Option<PathBuf> {
    let path = datalib_dag::config::root_config_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    let mut upgraded = text.clone();
    let mut done = Vec::new();
    for (why, what, rewrite) in REWRITES {
        match rewrite(&upgraded) {
            Ok(Some(next)) => {
                upgraded = next;
                done.push(what);
            }
            Ok(None) => {}
            Err(e) => tracing::error!(
                "config: {} {why}, and was left as it is: {e:#}. \
                 `datalib-migrate-config {} --force` shows the rewrite.",
                path.display(),
                root.display()
            ),
        }
    }
    if done.is_empty() {
        return None;
    }
    let bak = path.with_extension("toml.bak");
    let written = datalib_dag::config::write_owner_only(&bak, text.as_bytes())
        .and_then(|()| datalib_runtime::atomic::write_owner_only(&path, upgraded.as_bytes()));
    if let Err(e) = written {
        tracing::error!("config: could not migrate {}: {e}", path.display());
        return None;
    }
    tracing::warn!(
        "config: migrated {}: {}. The previous file is {}.",
        path.display(),
        done.join("; "),
        bak.display()
    );
    Some(bak)
}

/// What to do with the document index's tree from before the rename.
#[derive(Debug, PartialEq, Eq)]
pub enum IndexDir {
    /// Nothing there, or the config still writes the old tree.
    Leave,
    Move,
    /// Both trees exist; neither is the obvious one to keep.
    Both,
}

pub fn index_dir_plan(old_exists: bool, new_exists: bool, config_uses_old: bool) -> IndexDir {
    match (old_exists, new_exists) {
        _ if config_uses_old => IndexDir::Leave,
        (true, false) => IndexDir::Move,
        (true, true) => IndexDir::Both,
        (false, _) => IndexDir::Leave,
    }
}

/// Move `<root>/unified_index/` to `<root>/unified_document_index/` with
/// one rename, so the grid store and the qmd embeddings are kept rather
/// than rebuilt, and carry the loop's per-step rows (a step turned off,
/// its last success, its tree's version) over to the new step ids. Before
/// the loop starts: nothing may have either open.
pub async fn move_index_dir(root: &Path) {
    use datalib_core::layout::{
        RETIRED_UNIFIED_INDEX_DIR as OLD, UNIFIED_DOCUMENT_INDEX_DIR as NEW,
    };
    let config = datalib_dag::config::root_config_path(root);
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    // As written, not as loaded: a source the loader refuses for taking
    // the old id still owns the tree under it.
    let Ok(written) = datalib_dag::written::entries_as_written(&text) else {
        return;
    };
    let prefix = format!("{OLD}/");
    let config_uses_old = written.groups.iter().any(|g| g.id == OLD)
        || written.steps.iter().any(|s| s.id.starts_with(&prefix));
    let (old, new) = (root.join(OLD), root.join(NEW));
    match index_dir_plan(old.exists(), new.exists(), config_uses_old) {
        IndexDir::Leave => {}
        IndexDir::Move => match std::fs::rename(&old, &new) {
            Ok(()) => tracing::warn!("moved {} to {}", old.display(), new.display()),
            Err(e) => tracing::error!(
                "could not move {} to {}: {e}; the index is rebuilt under the new name",
                old.display(),
                new.display()
            ),
        },
        IndexDir::Both => tracing::error!(
            "both {} and {} exist; left as they are. The index steps write the second; \
             delete the first once nothing in it is wanted.",
            old.display(),
            new.display()
        ),
    }
    if config_uses_old || !datalib_core::layout::supervisor_db(root).exists() {
        return;
    }
    let renamed = match datalib_dag::supervisor::store::Store::open(root).await {
        Ok(store) => {
            let moved = store.rename_group_steps(OLD, NEW).await;
            store.close().await;
            moved
        }
        Err(e) => Err(e),
    };
    match renamed {
        Ok(0) => {}
        Ok(n) => tracing::warn!("supervisor: moved {n} rows from {OLD}/ to {NEW}/"),
        Err(e) => tracing::error!("supervisor: could not rename the {OLD}/ steps: {e:#}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_tree_moves_only_when_it_alone_exists_and_nothing_writes_it() {
        assert_eq!(index_dir_plan(true, false, false), IndexDir::Move);
        assert_eq!(index_dir_plan(true, true, false), IndexDir::Both);
        assert_eq!(index_dir_plan(false, true, false), IndexDir::Leave);
        assert_eq!(index_dir_plan(false, false, false), IndexDir::Leave);
        assert_eq!(index_dir_plan(true, false, true), IndexDir::Leave);
    }
}
