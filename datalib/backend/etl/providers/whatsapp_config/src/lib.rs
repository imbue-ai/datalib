//! Provider-owned config schema for the `whatsapp` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `WhatsappConfig` without linking the provider.

use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The whatsapp-owned slice of a `whatsapp` source. `backup` (the
/// decrypt+mirror path) is its one way in; an `ingest` step without it is
/// refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhatsappConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub backup: Option<WhatsAppSync>,
}

impl WhatsappConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The `backup` table: a WhatsApp Android crypt15 backup. Points at the
/// `WhatsApp/` directory the user pulls off their phone (containing
/// `Databases/msgstore.db.crypt15` and a sibling `Media/` tree of
/// plaintext attachments). The 32-byte root key is hex-encoded in the
/// env var named by `key_env_var` (defaults to
/// `WHATSAPP_BACKUP_DECRYPTION_KEY`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhatsAppSync {
    /// Directory containing `Databases/msgstore.db.crypt15` and the
    /// `Media/` tree.
    pub path: PathBuf,
    /// Env var holding the 32-byte root key as 64 hex chars. Defaults
    /// to `WHATSAPP_BACKUP_DECRYPTION_KEY`.
    #[serde(default)]
    pub key_env_var: Option<String>,
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type WhatsappRenderConfig = datalib_source_common::BareRenderConfig;

impl WhatsAppSync {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

impl datalib_source_common::IngestMethods for WhatsappConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("backup")];
}
