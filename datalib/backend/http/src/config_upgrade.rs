//! A config in a shape the server can bring forward unattended is
//! rewritten in place as soon as the server sees it: at boot, on a save
//! through the API, and whenever the file changes. The rewrites are
//! `datalib_migrate_config::UNATTENDED`, made in its order;
//! `docs/dev/config_model.md` §"The retired shapes" says what each does.
//! The text they replaced is kept as `config.toml.bak`.
//!
//! At boot only, [`move_index_dir`] then brings the index's tree and the
//! loop's record of its steps over to the current name,
//! [`move_index_step_dir`] does the same for the index step's own tree
//! and record, and [`move_case_folders`] moves each folder the config now
//! names in lower case to that spelling, before anything opens them.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::watch::{RootEvent, RootFrame};

/// A rewrite reads the file and writes it back, so a save landing between
/// the two would be lost: this process's writers of the config take turns.
static WRITING: Mutex<()> = Mutex::new(());

fn writing() -> MutexGuard<'static, ()> {
    WRITING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Write `text` as the root's config, already brought forward.
pub fn save(root: &Path, text: &str) -> std::io::Result<()> {
    let _turn = writing();
    let path = datalib_config::root_config_path(root);
    datalib_runtime::atomic::write_owner_only(&path, text.as_bytes())?;
    upgrade_in_turn(root);
    Ok(())
}

/// `text` with every rewrite that applies made, as [`save`] would leave
/// it on disk; `None` when none applies. A rewrite that refuses is
/// skipped, as at a save.
pub fn upgraded(text: &str) -> Option<String> {
    let upgraded = datalib_migrate_config::upgrade_unattended(text);
    (!upgraded.done.is_empty()).then_some(upgraded.text)
}

/// Rewrite the root's config if it is in an old shape, and say so in the
/// log. Returns where the old text went, when it did.
pub fn upgrade(root: &Path) -> Option<PathBuf> {
    let _turn = writing();
    upgrade_in_turn(root)
}

fn upgrade_in_turn(root: &Path) -> Option<PathBuf> {
    let path = datalib_config::root_config_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    let datalib_migrate_config::Unattended {
        text: upgraded,
        done,
        refused,
    } = datalib_migrate_config::upgrade_unattended(&text);
    for (why, e) in refused {
        tracing::error!(
            "config: {} {why}, and was left as it is: {e:#}. \
             `datalib-migrate-config {} --force` shows the rewrite.",
            path.display(),
            root.display()
        );
    }
    if done.is_empty() {
        return None;
    }
    let bak = path.with_extension("toml.bak");
    let written = datalib_config::write_owner_only(&bak, text.as_bytes())
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

/// Move the index's tree from each name it had before
/// (`layout::RETIRED_INDEX_DIRS`, newest first) to
/// `<root>/unified_grid_and_search_index/` with one rename, so the grid
/// store and the search index's vectors are kept rather than rebuilt, and
/// carry the loop's per-step rows (a step turned off, its last success,
/// its tree's version) over to the new step ids. Before the loop starts:
/// nothing may have either open.
pub async fn move_index_dir(root: &Path) {
    use datalib_core::layout::{RETIRED_INDEX_DIRS, UNIFIED_GRID_AND_SEARCH_INDEX_DIR as NEW};
    let config = datalib_config::root_config_path(root);
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    // As written, not as loaded: a source the loader refuses for taking
    // the old id still owns the tree under it.
    let Ok(written) = datalib_config::written::entries_as_written(&text) else {
        return;
    };
    for old in RETIRED_INDEX_DIRS {
        move_one_index_dir(root, &written, old, NEW).await;
    }
}

async fn move_one_index_dir(
    root: &Path,
    written: &datalib_config::written::WrittenEntries,
    old_id: &str,
    new_id: &str,
) {
    let prefix = format!("{old_id}/");
    let config_uses_old = written.groups.iter().any(|g| g.id == old_id)
        || written.steps.iter().any(|s| s.id.starts_with(&prefix));
    let (old, new) = (root.join(old_id), root.join(new_id));
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
            let moved = store.rename_group_steps(old_id, new_id).await;
            store.close().await;
            moved
        }
        Err(e) => Err(e),
    };
    match renamed {
        Ok(0) => {}
        Ok(n) => tracing::warn!("supervisor: moved {n} rows from {old_id}/ to {new_id}/"),
        Err(e) => tracing::error!("supervisor: could not rename the {old_id}/ steps: {e:#}"),
    }
}

/// Move the index step's tree from the name it had while its function was
/// `grid_index` to `unified_grid_and_search_index/unified_grid_and_search_index/`
/// with one rename, so the grid index's file, and the search tables and
/// cursors in it, are kept rather than rebuilt; and carry the loop's rows
/// for the step (turned off, its last success, its tree's version) over
/// to its new id. Before the loop starts: nothing may have the file open.
/// A config that still names the old step, because its rewrite was
/// refused, keeps the old tree.
pub async fn move_index_step_dir(root: &Path) {
    use datalib_core::layout::{
        INDEX_STEP_DIR, RETIRED_INDEX_STEP_DIR, UNIFIED_GRID_AND_SEARCH_INDEX_DIR as GROUP,
    };
    let config = datalib_config::root_config_path(root);
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let Ok(written) = datalib_config::written::entries_as_written(&text) else {
        return;
    };
    let old_id = format!("{GROUP}/{RETIRED_INDEX_STEP_DIR}");
    let new_id = format!("{GROUP}/{INDEX_STEP_DIR}");
    let config_uses_old = written.steps.iter().any(|s| s.id == old_id);
    let (old, new) = (root.join(&old_id), root.join(&new_id));
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
            "both {} and {} exist; left as they are. The index step writes the second; \
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
            let moved = store.rename_step(&old_id, &new_id).await;
            store.close().await;
            moved
        }
        Err(e) => Err(e),
    };
    match renamed {
        Ok(0) => {}
        Ok(n) => tracing::warn!("supervisor: moved {n} rows from {old_id} to {new_id}"),
        Err(e) => tracing::error!("supervisor: could not rename {old_id}: {e:#}"),
    }
}

/// What to do about the folder `want`, given the names in the folder
/// that should hold it.
#[derive(Debug, PartialEq, Eq)]
pub enum CaseFolder {
    /// `want` is there as spelled, or no spelling of it is.
    Leave,
    /// Only this other spelling is there: move it to `want`.
    Move(String),
    /// `want` and this other spelling are both there; neither is the
    /// obvious one to keep.
    Both(String),
}

pub fn case_folder(names: &[String], want: &str) -> CaseFolder {
    let other = names
        .iter()
        .find(|n| n.as_str() != want && n.eq_ignore_ascii_case(want));
    match (names.iter().any(|n| n == want), other) {
        (_, None) => CaseFolder::Leave,
        (false, Some(o)) => CaseFolder::Move(o.clone()),
        (true, Some(o)) => CaseFolder::Both(o.clone()),
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Move each folder a group or step of the config names to the spelling
/// the config now uses, one path segment at a time, and carry whether
/// each step under it was turned off. Ids that name folders are lower
/// case, and an older config could have capitals in one. Only the
/// folders move: the steps under them run again as new steps, so the
/// grid and the search index take the source under its new id. Before
/// the loop starts: nothing may have a store open.
pub async fn move_case_folders(root: &Path) {
    let config = datalib_config::root_config_path(root);
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let Ok(written) = datalib_config::written::entries_as_written(&text) else {
        return;
    };
    let trees: std::collections::BTreeSet<&str> = written
        .groups
        .iter()
        .map(|g| g.id.as_str())
        .chain(written.steps.iter().map(|s| s.id.as_str()))
        // An id the rewrite could not lower is not a spelling to move to.
        .filter(|id| !id.chars().any(|c| c.is_ascii_uppercase()))
        .collect();
    let mut moved: Vec<(String, String)> = Vec::new();
    for tree in trees {
        let segments: Vec<&str> = tree.split('/').collect();
        for depth in 0..segments.len() {
            let parent = segments[..depth]
                .iter()
                .fold(root.to_path_buf(), |p, s| p.join(s));
            let want = segments[depth];
            match case_folder(&names_in(&parent), want) {
                CaseFolder::Leave => {}
                CaseFolder::Move(from) => {
                    match std::fs::rename(parent.join(&from), parent.join(want)) {
                        Ok(()) => {
                            let at = segments[..depth].join("/");
                            let join = |s: &str| {
                                if at.is_empty() {
                                    s.to_string()
                                } else {
                                    format!("{at}/{s}")
                                }
                            };
                            tracing::warn!(
                                "moved {} to {}",
                                parent.join(&from).display(),
                                parent.join(want).display()
                            );
                            moved.push((join(&from), join(want)));
                        }
                        Err(e) => tracing::error!(
                            "could not move {} to {}: {e}; the step under it starts from nothing",
                            parent.join(&from).display(),
                            parent.join(want).display()
                        ),
                    }
                }
                CaseFolder::Both(other) => tracing::error!(
                    "both {} and {} exist; left as they are. The config names the second; \
                     delete the first once nothing in it is wanted.",
                    parent.join(&other).display(),
                    parent.join(want).display()
                ),
            }
        }
    }
    if moved.is_empty() || !datalib_core::layout::supervisor_db(root).exists() {
        return;
    }
    let store = match datalib_dag::supervisor::store::Store::open(root).await {
        Ok(store) => store,
        Err(e) => {
            tracing::error!("supervisor: could not carry the moved steps' switches: {e:#}");
            return;
        }
    };
    for (from, to) in &moved {
        if let Err(e) = store.rename_switches(from, to).await {
            tracing::error!("supervisor: could not carry the switches of {from}/ to {to}/: {e:#}");
        }
    }
    store.close().await;
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
    fn a_folder_moves_to_its_one_spelling_only_when_that_is_not_there() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(case_folder(&names(&["email"]), "email"), CaseFolder::Leave);
        assert_eq!(case_folder(&names(&["other"]), "email"), CaseFolder::Leave);
        assert_eq!(
            case_folder(&names(&["Email", "other"]), "email"),
            CaseFolder::Move("Email".into())
        );
        assert_eq!(
            case_folder(&names(&["email", "EMAIL"]), "email"),
            CaseFolder::Both("EMAIL".into())
        );
    }

    /// A config brought to lower case finds its data where the old
    /// spelling left it: the folders move, the files in them with them,
    /// and a step someone turned off stays off.
    #[tokio::test]
    async fn the_old_spellings_folders_move_and_their_switches_follow() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(
            datalib_config::root_config_path(root),
            "[[groups]]\nid = \"email\"\n\n\
             [[steps]]\ngroup = \"email\"\nfunction = \"ingest\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"g/raw\"\ncommand = \"y\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("Email/ingest")).unwrap();
        std::fs::write(root.join("Email/ingest/store"), "held").unwrap();
        std::fs::create_dir_all(root.join("g/Raw")).unwrap();
        let store = datalib_dag::supervisor::store::Store::open(root)
            .await
            .unwrap();
        store.turn_off("Email/ingest", "thad").await.unwrap();
        store.close().await;

        move_case_folders(root).await;

        assert_eq!(names_in(root).iter().filter(|n| *n == "Email").count(), 0);
        assert!(names_in(root).contains(&"email".to_string()));
        assert_eq!(names_in(&root.join("g")), ["raw"]);
        assert_eq!(
            std::fs::read_to_string(root.join("email/ingest/store")).unwrap(),
            "held"
        );
        let store = datalib_dag::supervisor::store::Store::open(root)
            .await
            .unwrap();
        let off: Vec<String> = store.turned_off().await.unwrap().into_keys().collect();
        store.close().await;
        assert_eq!(off, ["email/ingest"]);
    }

    #[test]
    fn the_old_tree_moves_only_when_it_alone_exists_and_nothing_writes_it() {
        assert_eq!(index_dir_plan(true, false, false), IndexDir::Move);
        assert_eq!(index_dir_plan(true, true, false), IndexDir::Both);
        assert_eq!(index_dir_plan(false, true, false), IndexDir::Leave);
        assert_eq!(index_dir_plan(false, false, false), IndexDir::Leave);
        assert_eq!(index_dir_plan(true, false, true), IndexDir::Leave);
    }
}
