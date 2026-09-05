//! HTTP playback fixture synthesis for the Beeper provider.

use std::path::{Path, PathBuf};

use anyhow::Result;

use datalib_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct BeeperSynth {
    #[allow(dead_code)]
    input: PathBuf,
}

impl BeeperSynth {
    pub fn new(input: PathBuf) -> Self {
        Self { input }
    }
}

impl Synthesizer for BeeperSynth {
    fn name(&self) -> &'static str {
        "beeper"
    }

    fn synthesize(&self, _out: &Path) -> Result<SynthesizeReport> {
        Ok(SynthesizeReport::default())
    }
}
