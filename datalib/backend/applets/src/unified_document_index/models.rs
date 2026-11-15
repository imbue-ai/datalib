//! Whether the pinned embedding model is in place for a search by meaning.
//! The query embedder asks here before it loads the model, and loads
//! nothing until it is verified.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::Result;
use datalib_qmd_indexer::{ModelsNotReady, VerifiedModels};
use datalib_runtime::qmd::EMBED_MODELS;

#[derive(Clone)]
pub(super) struct ModelGate(Arc<Inner>);

struct Inner {
    root: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
enum State {
    /// Nothing has asked yet, or there was no index to provision for.
    Idle,
    Provisioning,
    Verified(PathBuf),
    Failed(String),
}

impl ModelGate {
    pub(super) fn new(root: &Path) -> Self {
        Self(Arc::new(Inner {
            root: root.to_path_buf(),
            state: Mutex::new(State::Idle),
            changed: Condvar::new(),
        }))
    }

    /// Verifies, and on a cold cache fetches, the embedding model on a
    /// thread of its own: a 300 MB download kept off the request path.
    pub(super) fn warm(&self) {
        if self.claim() {
            let inner = self.0.clone();
            std::thread::spawn(move || inner.provision());
        }
    }

    /// Until the embedding model is in place or has failed. Nothing has
    /// to be under way: this returns at once when nothing is.
    pub(super) fn wait_while_provisioning(&self) {
        let state = self.0.lock();
        drop(
            self.0
                .changed
                .wait_while(state, |s| matches!(s, State::Provisioning))
                .unwrap_or_else(|e| e.into_inner()),
        );
    }

    /// The embedding model alone, on this thread: a one-off search waits
    /// for it here.
    pub(super) fn provision_now(&self) {
        if self.claim() {
            self.0.provision();
        }
    }

    /// Moves to `Provisioning` unless something already is, or the models
    /// are in: whether the caller is the one to provision.
    fn claim(&self) -> bool {
        let mut state = self.0.lock();
        match *state {
            State::Provisioning | State::Verified(_) => false,
            State::Idle | State::Failed(_) => {
                *state = State::Provisioning;
                true
            }
        }
    }
}

impl VerifiedModels for ModelGate {
    fn dir(&self, deadline: Instant) -> Result<PathBuf> {
        let failed = match &*self.0.lock() {
            State::Failed(e) => Some(e.clone()),
            _ => None,
        };
        // Asked first by a search, or again after a failure, which this
        // search reports while the next one sees how the new try went.
        if self.claim() {
            let inner = self.0.clone();
            std::thread::spawn(move || inner.provision());
        }
        if let Some(e) = failed {
            return Err(ModelsNotReady(failed_because(&e)).into());
        }
        let state = self.0.lock();
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |s| matches!(s, State::Provisioning),
            )
            .unwrap_or_else(|e| e.into_inner());
        let why = match &*state {
            State::Verified(dir) => return Ok(dir.clone()),
            State::Idle => "there is no search index to embed for yet".to_string(),
            State::Provisioning => "the search model is still being fetched and verified (about \
                 300 MB the first time); searching by meaning starts once it is in place, and \
                 the Words and Fields tabs work now"
                .to_string(),
            State::Failed(e) => failed_because(e),
        };
        Err(ModelsNotReady(why).into())
    }
}

fn failed_because(e: &str) -> String {
    format!(
        "the search model could not be put in place, so nothing searches by meaning: {e}. \
         The next search tries again; `datalib-step pull-models` fetches it by hand"
    )
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, state: State) {
        *self.lock() = state;
        self.changed.notify_all();
    }

    fn provision(&self) {
        let dir = match link_models(&self.root) {
            Ok(Some(dir)) => dir,
            Ok(None) => return self.set(State::Idle),
            Err(e) => return self.set(State::Failed(format!("{e:#}"))),
        };
        let embedded = ensure(&dir, EMBED_MODELS).and_then(|missing| {
            anyhow::ensure!(
                missing.is_empty(),
                "{} is not in {} and fetching is switched off ({})",
                missing.join(", "),
                dir.display(),
                datalib_qmd_models::NO_FETCH_ENV
            );
            Ok(())
        });
        match embedded {
            Ok(()) => self.set(State::Verified(dir)),
            Err(e) => {
                tracing::error!(
                    dir = %dir.display(),
                    error = %format!("{e:#}"),
                    "could not provision the embedding model; nothing searches by meaning until it is in place"
                );
                self.set(State::Failed(format!("{e:#}")));
            }
        }
    }
}

/// Verifies `models` in `dir`, fetching what is not there if fetching is
/// on, and answers the ones still not there.
fn ensure<'a>(dir: &Path, models: &'a [datalib_runtime::qmd::PinnedModel]) -> Result<Vec<&'a str>> {
    let outcomes =
        datalib_qmd_models::ensure_models(dir, models, datalib_qmd_models::Fetch::from_env())?;
    Ok(datalib_qmd_models::missing(models, &outcomes))
}

/// The directory the pinned models belong in for `root`. None when there
/// is nothing to search: no grid index means no sync has run, and no
/// search to warm.
fn link_models(root: &Path) -> Result<Option<PathBuf>> {
    if !datalib_runtime::layout::grid_index_db(root).exists() {
        tracing::info!("no grid index yet; nothing searches by meaning until the first sync");
        return Ok(None);
    }
    Ok(Some(datalib_qmd_models::models_dir_for(root)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(50)
    }

    fn not_ready(gate: &ModelGate) -> String {
        let err = gate.dir(soon()).unwrap_err();
        assert!(err.downcast_ref::<ModelsNotReady>().is_some(), "{err:#}");
        format!("{err:#}")
    }

    /// The regression: a search while the models were still downloading
    /// started qmd, which fetched its own unpinned copy. Until they are
    /// verified the gate answers no directory at all.
    #[test]
    fn no_directory_while_the_models_are_being_provisioned() {
        let gate = ModelGate::new(Path::new("/nonexistent"));
        *gate.0.lock() = State::Provisioning;
        assert!(not_ready(&gate).contains("still being fetched"));
    }

    #[test]
    fn a_failed_provision_says_why_and_is_tried_again() {
        let gate = ModelGate::new(Path::new("/nonexistent"));
        *gate.0.lock() = State::Failed("the network is down".into());
        let why = not_ready(&gate);
        assert!(why.contains("the network is down"), "{why}");
        // The search that saw the failure started another try; the root
        // has no index, so that try finds nothing to provision.
        assert!(matches!(*gate.0.lock(), State::Idle | State::Provisioning));
    }

    /// A search waiting on a provision that finishes in time gets its
    /// answer rather than an error.
    #[test]
    fn a_search_waits_for_a_provision_that_finishes() {
        let gate = ModelGate::new(Path::new("/nonexistent"));
        *gate.0.lock() = State::Provisioning;
        let inner = gate.0.clone();
        std::thread::spawn(move || inner.set(State::Verified("/models".into())));
        let dir = gate.dir(Instant::now() + Duration::from_secs(30)).unwrap();
        assert_eq!(dir, Path::new("/models"));
    }
}
