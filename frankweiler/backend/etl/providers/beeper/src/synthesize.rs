//! HTTP playback fixture synthesis for the Beeper provider.
//!
//! Placeholder: synth is only needed when we wire Beeper into the
//! hermetic Bazel genrule path. Until then the trait impl just reports
//! zero fixtures so the orchestrator can iterate over us harmlessly.

use std::path::{Path, PathBuf};

use anyhow::Result;

use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

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
