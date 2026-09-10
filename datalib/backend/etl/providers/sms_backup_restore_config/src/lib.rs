//! Provider-owned config schema for the `sms_backup_restore` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `SmsBackupRestoreConfig` without linking the provider.

use datalib_source_common::{LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// The sms_backup_restore-owned slice of an `sms_backup_restore` source.
/// `backup` — the app's XML export, or a directory of them — is its one
/// way in, and its only knob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmsBackupRestoreConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub backup: Option<LocalPath>,
}

impl SmsBackupRestoreConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type SmsBackupRestoreRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for SmsBackupRestoreConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("backup")];
}
