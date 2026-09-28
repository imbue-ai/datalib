//! Where a running loop re-reads its graph, so a source added to the
//! config, or a step edited in it, is taken on by the busy period already
//! running rather than the next one. How the loop carries its state from
//! one graph to the next is in `round.rs`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config;
use crate::graph::Graph;

/// Asked when a busy period starts and whenever the config is announced
/// as changed; the loop compares what it gets with the graph it has.
pub trait GraphSource: Send + Sync {
    fn load(&self) -> Result<Graph>;
}

pub struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl GraphSource for ConfigFile {
    fn load(&self) -> Result<Graph> {
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        let checked = config::check_text(&text);
        anyhow::ensure!(
            !checked.is_fatal(),
            "{} is not a config: nothing in it could be read",
            self.path.display()
        );
        Ok(checked.graph)
    }
}
