//! Shared cross-provider knobs for `ingest::fetch`.

/// Cross-provider download-time knobs.
#[derive(Debug, Clone, Default)]
pub struct DownloadControl {
    /// How often a download seals what it has written so far, so a consumer
    /// can start on it before the whole fetch finishes.
    ///
    /// `None` means the default cadence. The dial is the user's — how much
    /// latency to trade for how much `dolt_log` is their call.
    pub checkpoint_cadence: Option<crate::checkpointer::Cadence>,

    /// Raised when the step has been asked to stop (SIGINT). A fetch loop
    /// checks it before starting a unit of work and ends the run early;
    /// the seal path seals at the next consistent point regardless of
    /// cadence; a backoff sleep returns at once. See [`crate::stop`].
    pub stop: crate::stop::StopFlag,
}
