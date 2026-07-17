//! Provider-owned config schema for the `signal_backup` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `SignalConfig` without linking the provider.

use std::path::PathBuf;

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The signal-owned slice of a `signal_backup` source. `sync:` present →
/// managed (the download path: decrypt the newest snapshot under
/// `snapshot_dir`); absent → render-only over an already-ingested raw
/// store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<SignalSync>,
}

impl SignalConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Signal-Android directory-format backup sync knobs. The extractor finds the
/// newest `signal-backup-*` subdir under `snapshot_dir`, decrypts it using the
/// AEP read from `$aep_env_var` at download time, and UPSERTs frames into a
/// doltlite raw store. No network; no credentials in this struct — the secret
/// lives in the user's shell (or .envrc.private).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SignalSync {
    /// Directory containing one or more `signal-backup-*` snapshot
    /// subdirs (Signal Android's "Save backup" target). The newest is
    /// ingested. Required; the source's `input_path` is reserved for
    /// the raw doltlite store and defaults to `${data_root}/raw/<name>`.
    pub snapshot_dir: PathBuf,
    /// Env var holding the AEP (Account Entropy Pool). Defaults to
    /// `SIGNAL_BACKUP_PASSPHRASE` when omitted. Overridable so a multi-account
    /// setup can scope per-account secrets at the shell layer.
    #[serde(default)]
    pub aep_env_var: Option<String>,
    /// Period-bucketing knob for the rendered markdown tree —
    /// `month` (default), `day`, `year`, or `all`. Shared across
    /// every chat provider via `frankweiler_etl::periodize::Period`;
    /// signal accepts the same strings beeper does so a unified
    /// config can tune both at once.
    #[serde(default)]
    pub period: Option<String>,
}
