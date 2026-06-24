//! Provider-owned config schema for the `google_takeout` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `GoogleTakeoutConfig` without linking the provider.
//!
//! `GoogleTakeoutSync` is the per-feed opt-in block; it replaces the copy
//! that used to live in `frankweiler-core`'s `config.rs`. The provider's
//! `plan()` maps it field-for-field onto
//! `frankweiler_etl_google_takeout::extract::SyncFlags`, so the SyncFlags
//! duplication is now provider-owned rather than orchestrator-owned.

use serde::{Deserialize, Serialize};

/// The google_takeout-owned slice of a `google_takeout` source. File-backed:
/// `input_path:` points at the unzipped Takeout root. `sync:` opts into
/// individual feeds; absent → all feeds off (default).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleTakeoutConfig {
    #[serde(default)]
    pub sync: Option<GoogleTakeoutSync>,
}

impl GoogleTakeoutConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Per-feed opt-in switches for a Google Takeout export. Mirrors
/// `frankweiler_etl_google_takeout::extract::SyncFlags` (the provider's
/// `plan()` maps one to the other); defaults are all `false` so a fresh user
/// enables each feed consciously.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct GoogleTakeoutSync {
    pub maps_reviews: bool,
    pub maps_saved_places: bool,
    pub maps_photos: bool,
    pub youtube_watch_history: bool,
    pub youtube_subscriptions: bool,
    pub google_chat: bool,
    pub gemini_apps: bool,
    /// Google Voice (`Voice/` subtree): texts, voicemails, calls, bills.
    pub google_voice: bool,
    /// When `google_voice` is on, also process `Voice/Spam/`.
    pub google_voice_include_spam: bool,
}
