//! Provider-owned config schema for the `signal` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `SignalConfig` without linking the provider.

use std::path::PathBuf;

use datalib_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "backup",
        toml: include_str!("../examples/minimal_backup.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "backup",
        toml: include_str!("../examples/complete_backup.toml"),
    },
];

/// The signal-owned slice of a `signal` source. `backup` (decrypt every
/// snapshot under its `path`) is its one way in; an `ingest`
/// step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub backup: Option<SignalSync>,
}

impl SignalConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the signal **render** step. Split from [`SignalConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// How much time each rendered document spans — `month` (default),
    /// `day`, `year`, or `all`. Every chat source offers the same choice.
    #[serde(default)]
    pub period: Option<String>,
}

/// The `backup` table. The extractor reads every `signal-backup-*`
/// subdir under `path` the store does not hold, oldest first, decrypting
/// each with the AEP read from `$aep_env_var` at download time, one
/// commit per snapshot. No network; no credentials in this struct — the secret
/// lives in the user's shell (or .envrc.private).
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalSync {
    /// Directory containing one or more `signal-backup-*` snapshot
    /// subdirs (Signal Android's "Save backup" target). Each snapshot is
    /// kept as one version of the store, and the newest is the current one.
    pub path: PathBuf,
    /// The environment variable holding the backup passphrase (Signal's
    /// AEP, Account Entropy Pool); `SIGNAL_BACKUP_PASSPHRASE` when omitted.
    /// Name a different one per source to keep two accounts' passphrases apart.
    #[serde(default)]
    pub aep_env_var: Option<String>,
    /// Where `period` used to go. It now belongs on the render step's
    /// params; an older config that still sets it here loads, and the
    /// ingest step refuses it with a pointer to the render step. Out of
    /// the schema, so `datalib-step help` does not offer it.
    #[serde(default)]
    #[schemars(skip)]
    pub period: Option<String>,
}

impl SignalSync {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

impl datalib_source_common::IngestMethods for SignalConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("backup")];
}
