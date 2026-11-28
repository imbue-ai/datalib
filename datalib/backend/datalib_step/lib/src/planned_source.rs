//! What a planned source is: the step's processors and the facts about
//! the source the step drivers need. `datalib-step`'s `dispatch` makes
//! one from a source's type and params; the drivers here run it.

use std::path::PathBuf;

use datalib_etl::processor::DataProcessor;
use datalib_etl_render::processor::RenderProcessor;
use datalib_source_common::{DownloadParams, Reach};

use crate::source_type::SourceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ingest,
    Render,
}

/// A normalized, planned source: the step's processors plus the
/// resolved envelope facts the step driver needs.
pub struct PlannedSource {
    pub name: String,
    pub source_type: SourceType,
    /// The raw store: the ingest step's own tree, or for a render the
    /// tree its input names.
    pub raw_path: PathBuf,
    /// Whether the method the ingest step's params hold reaches a live
    /// origin or reads files on disk. `None` for a render.
    pub reach: Option<Reach>,
    /// Resolved rate-limit give-up bounds for the ingest step.
    pub download_params: DownloadParams,
    pub processors: Processors,
}

/// A planned source's processors, which are of a different type per
/// phase: download and render share no trait, because they share no run
/// context.
pub enum Processors {
    Ingest(Vec<Box<dyn DataProcessor>>),
    /// A download whose params say `common.download = false`: nothing to
    /// run, and the raw store it has is what it publishes.
    Kept,
    Render(Vec<Box<dyn RenderProcessor>>),
}

impl Processors {
    pub fn len(&self) -> usize {
        match self {
            Processors::Ingest(p) => p.len(),
            Processors::Kept => 0,
            Processors::Render(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for PlannedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedSource")
            .field("name", &self.name)
            .field("source_type", &self.source_type)
            .field("raw_path", &self.raw_path)
            .field("processors", &self.processors.len())
            .finish_non_exhaustive()
    }
}
