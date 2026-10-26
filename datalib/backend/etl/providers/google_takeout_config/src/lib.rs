//! Provider-owned config schema for the `google_takeout` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `GoogleTakeoutConfig` without linking the provider.

use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "export",
        toml: include_str!("../examples/minimal_export.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "export",
        toml: include_str!("../examples/complete_export.toml"),
    },
];

/// The google_takeout-owned slice of a `google_takeout` source. `export`
/// — where the Takeout is, plus which of its feeds to read — is its one
/// way in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoogleTakeoutConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub export: Option<GoogleTakeoutSync>,
}

impl GoogleTakeoutConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The `export` table: where the Takeout is, and per-feed
/// opt-in switches. The switches mirror
/// `datalib_etl_google_takeout::ingest::SyncFlags` (the provider's
/// `plan()` maps one to the other); they default to `false` so a fresh
/// user enables each feed consciously.
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct GoogleTakeoutSync {
    /// The unpacked `Takeout/` folder, or a folder holding the `.zip` or
    /// `.tgz` parts of one export.
    pub path: PathBuf,
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
    /// Maps lists under `Saved/`, with the notes on their places.
    pub maps_saved_lists: bool,
    /// My Maps maps: pins, lines and shapes, and what was written on them.
    pub my_maps: bool,
    /// Play Books highlights, notes, bookmarks and listening positions.
    pub play_books: bool,
}

impl GoogleTakeoutSync {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type GoogleTakeoutRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GoogleTakeoutConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("export")];
}
