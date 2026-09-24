//! Where a running loop re-reads its graph, so a source added to the
//! config, or a step edited in it, is taken on by the busy period already
//! running rather than the next one. How the loop carries its state from
//! one graph to the next is `round.rs`'s `Swap`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config;
use crate::graph::Graph;

pub trait GraphSource: Send + Sync {
    /// Moves whenever what [`GraphSource::load`] would build might have.
    /// Asked on every mailbox poll, so it must be cheap.
    fn version(&self) -> Result<String>;

    /// The graph as the source describes it now, and the version it was
    /// built from.
    fn load(&self) -> Result<(String, Graph)>;

    /// A file whose writes mean the graph may have changed, for the loop
    /// to wake on.
    fn watched(&self) -> Option<PathBuf> {
        None
    }
}

/// A config file, read whole: its text is its version.
pub struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> Result<String> {
        std::fs::read_to_string(&self.path).with_context(|| format!("read {}", self.path.display()))
    }
}

impl GraphSource for ConfigFile {
    fn version(&self) -> Result<String> {
        self.read()
    }

    fn load(&self) -> Result<(String, Graph)> {
        let text = self.read()?;
        let checked = config::check_text(&text);
        anyhow::ensure!(
            !checked.is_fatal(),
            "{} is not a config: nothing in it could be read",
            self.path.display()
        );
        Ok((text, checked.graph))
    }

    fn watched(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }
}
