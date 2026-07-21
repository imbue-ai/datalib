//! Provider-owned config schema for the `whatsapp_backup` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `WhatsappConfig` without linking the provider.

use std::path::PathBuf;

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The whatsapp-owned slice of a `whatsapp_backup` source. `sync:` present →
/// the decrypt+mirror download path; absent → render-only over an
/// already-on-disk raw store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WhatsappConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<WhatsAppSync>,
}

impl WhatsappConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// WhatsApp Android crypt15 backup. Points at the `WhatsApp/` directory
/// the user pulls off their phone (containing `Databases/msgstore.db.crypt15`
/// and a sibling `Media/` tree of plaintext attachments). The 32-byte
/// root key is hex-encoded in the env var named by `key_env_var`
/// (defaults to `WHATSAPP_BACKUP_DECRYPTION_KEY`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhatsAppSync {
    /// Directory containing `Databases/msgstore.db.crypt15` and the
    /// `Media/` tree. Required; the source's `input_path` is reserved
    /// for the raw doltlite store and defaults to
    /// `${data_root}/raw/<name>`.
    pub backup_dir: PathBuf,
    /// Env var holding the 32-byte root key as 64 hex chars. Defaults
    /// to `WHATSAPP_BACKUP_DECRYPTION_KEY`.
    #[serde(default)]
    pub key_env_var: Option<String>,
}
