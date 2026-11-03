//! Whether qmd's pinned models are in place for a search by meaning. The
//! query embedder asks here before it starts qmd, and starts nothing
//! until the embedding model is verified, because qmd fetches a model it
//! does not find, unpinned.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::Result;
use datalib_qmd_indexer::{ModelsNotReady, VerifiedModels};
use datalib_runtime::qmd::{EMBED_MODELS, PINNED_MODELS};

#[derive(Clone)]
pub(super) struct ModelGate(Arc<Inner>);

struct Inner {
    root: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
enum State {
    /// Nothing has asked yet, or there was no qmd index to provision for.
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

    /// Verifies, and on a cold cache fetches, every pinned model on a
    /// thread of its own: a 2 GB download kept off the request path. A
    /// search by meaning can start once the embedding model is in.
    pub(super) fn warm(&self) {
        if self.claim() {
            let inner = self.0.clone();
            std::thread::spawn(move || inner.provision(PINNED_MODELS));
        }
    }

    /// The embedding model alone, on this thread: a one-off search waits
    /// for it here.
    pub(super) fn provision_now(&self) {
        if self.claim() {
            self.0.provision(EMBED_MODELS);
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
            std::thread::spawn(move || inner.provision(EMBED_MODELS));
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

    /// The embedding model first, since a search waits on it alone; then
    /// the rest of `models`, which only the log hears about.
    fn provision(&self, models: &[datalib_runtime::qmd::PinnedModel]) {
        let dir = match link_models(&self.root) {
            Ok(Some(dir)) => dir,
            Ok(None) => return self.set(State::Idle),
            Err(e) => return self.set(State::Failed(format!("{e:#}"))),
        };
        let (embed, rest) = models.split_at(EMBED_MODELS.len().min(models.len()));
        let embedded = ensure(&dir, embed).and_then(|missing| {
            anyhow::ensure!(
                missing.is_empty(),
                "{} is not in {} and fetching is switched off ({})",
                missing.join(", "),
                dir.display(),
                datalib_qmd_models::NO_FETCH_ENV
            );
            Ok(())
        });
        if let Err(e) = embedded {
            tracing::error!(
                dir = %dir.display(),
                error = %format!("{e:#}"),
                "could not provision qmd's embedding model; nothing searches by meaning until it is in place"
            );
            return self.set(State::Failed(format!("{e:#}")));
        }
        self.set(State::Verified(dir.clone()));
        match ensure(&dir, rest) {
            Ok(missing) if missing.is_empty() => {}
            Ok(missing) => tracing::warn!(
                missing = %missing.join(","),
                dir = %dir.display(),
                switch = datalib_qmd_models::NO_FETCH_ENV,
                "not fetching qmd's other models because fetching is switched off"
            ),
            Err(e) => tracing::error!(
                dir = %dir.display(),
                error = %format!("{e:#}"),
                "could not provision qmd's other models; `datalib-step pull-models` fetches them"
            ),
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

/// Points the root's qmd state at the shared model cache, and answers the
/// directory the pinned models belong in. None when there is nothing to
/// point: no index means no sync has run, and no search to warm.
fn link_models(root: &Path) -> Result<Option<PathBuf>> {
    // The first sync creates the directory and the indexer links it;
    // this call is the belt for a root the indexer has not touched in
    // this incarnation.
    if !datalib_unified_document_index::qmd::qmd_index_path(root).exists() {
        tracing::info!(
            "no qmd index yet; free-text search finds nothing until the first sync builds one"
        );
        return Ok(None);
    }
    let qmd_dir = datalib_unified_document_index::qmd::qmd_state_dir(root);
    let models_dir = datalib_qmd_indexer::default_models_dir();
    std::fs::create_dir_all(&models_dir)?;
    datalib_qmd_indexer::ensure_models_symlink(&qmd_dir, &models_dir)?;
    Ok(Some(datalib_qmd_models::effective_models_dir(
        &qmd_dir,
        &models_dir,
    )))
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
