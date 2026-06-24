//! Provider-owned config schema for the `sms_backup_restore` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `SmsBackupRestoreConfig` without linking the provider.
//!
//! The `sms_backup_restore` source has no `sync:` block and no provider knobs —
//! it's a purely file-backed ingest driven by `input_path`. So the config
//! struct is empty; it exists only to make every provider's `plan(common,
//! config)` shape uniform.

use serde::{Deserialize, Serialize};

/// The sms_backup_restore-owned slice of an `sms_backup_restore` source. Empty:
/// the provider has no configurable knobs (file-backed, `input_path`-driven).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmsBackupRestoreConfig {}

impl SmsBackupRestoreConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
