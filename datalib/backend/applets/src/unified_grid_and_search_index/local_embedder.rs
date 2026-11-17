//! A search's text embedded in this process, by the model and prompt the
//! search index's vectors were made with (`datalib_embedder`), rather than
//! before. The model loads at the first search by meaning and stays loaded.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::models::VerifiedModels;
use anyhow::{Context, Result};
use datalib_embedder::{Device, Embedder};

pub(super) struct LocalEmbedder {
    models: Arc<dyn VerifiedModels>,
    /// How long a search waits for the model to be verified.
    deadline: Duration,
    live: Mutex<Option<Embedder>>,
}

impl LocalEmbedder {
    pub(super) fn new(models: Arc<dyn VerifiedModels>, deadline: Duration) -> Self {
        Self {
            models,
            deadline,
            live: Mutex::new(None),
        }
    }

    pub(super) fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let embedder = match &mut *live {
            Some(embedder) => embedder,
            None => {
                let dir = self.models.dir(Instant::now() + self.deadline)?;
                let file = dir.join(datalib_runtime::models::EMBED_MODELS[0].cache_name());
                let opened = Embedder::open(&file, Device::Best, 0)
                    .with_context(|| format!("load the search model {}", file.display()))?;
                tracing::info!(
                    device = %opened.device(),
                    cuda = datalib_embedder::cuda_module_report().unwrap_or("not looked for"),
                    "loaded the search model"
                );
                live.insert(opened)
            }
        };
        embedder.embed(&datalib_search_index::prompt::query(text))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::models::ModelsNotReady;
    use super::*;

    struct NotYet;

    impl VerifiedModels for NotYet {
        fn dir(&self, _deadline: Instant) -> Result<PathBuf> {
            Err(ModelsNotReady("still being fetched".into()).into())
        }
    }

    /// A search by meaning before the model is verified fails with the
    /// gate's answer, and loads nothing.
    #[test]
    fn a_query_before_the_model_is_verified_loads_nothing() {
        let embedder = LocalEmbedder::new(Arc::new(NotYet), Duration::from_millis(10));
        let err = embedder.embed("warp").unwrap_err();
        assert!(err.downcast_ref::<ModelsNotReady>().is_some(), "{err:#}");
        assert!(embedder.live.lock().unwrap().is_none());
    }
}
