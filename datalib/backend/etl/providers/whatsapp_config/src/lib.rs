//! Provider-owned config schema for the `whatsapp` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `WhatsappConfig` without linking the provider.

use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Tables folded out by [`WhatsappConfig::skip_churn`]. `props` is the
/// app's own key/value settings, which WhatsApp rewrites (and renumbers)
/// between backups; `backup_changes` is its "what changed since the last
/// backup" log, which the mirror's `dolt_log` supersedes; `frequent` is
/// per-contact usage counters. Measured on two real backups a week
/// apart, these were everything that moved without a message doing so.
pub const CHURN_TABLE_PATTERNS: &[&str] = &["props", "backup_changes", "frequent"];

/// The whatsapp-owned slice of a `whatsapp` source. `backup` (the
/// decrypt+mirror path) is its one way in; an `ingest` step without it is
/// refused (`IngestMethods` below). The rest are the mirror engine's
/// knobs, the same ones `apple_photos` exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WhatsappConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    pub common: SourceCommon,
    pub backup: Option<WhatsAppSync>,

    /// Table-name globs to mirror. Default `["*"]` — every table in
    /// msgstore. `*` and `?` are the only metacharacters.
    pub include_tables: Vec<String>,
    /// Table-name globs to skip, applied after [`Self::include_tables`].
    pub exclude_tables: Vec<String>,
    /// `Table.column` globs to drop from the mirror.
    pub exclude_columns: Vec<String>,
    /// Fold [`CHURN_TABLE_PATTERNS`] into the exclusions. On by default:
    /// with it off, a backup nobody messaged in still commits every run.
    pub skip_churn: bool,
    /// Collect unreachable chunks (`dolt_gc()`) at the start of each run.
    pub gc: bool,
}

impl Default for WhatsappConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
            backup: None,
            include_tables: vec!["*".to_string()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            skip_churn: true,
            gc: false,
        }
    }
}

impl WhatsappConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.include_tables.is_empty() {
            anyhow::bail!("include_tables is empty: nothing would be mirrored");
        }
        Ok(())
    }

    pub fn effective_excluded_tables(&self) -> Vec<String> {
        let mut out = self.exclude_tables.clone();
        if self.skip_churn {
            out.extend(CHURN_TABLE_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
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
