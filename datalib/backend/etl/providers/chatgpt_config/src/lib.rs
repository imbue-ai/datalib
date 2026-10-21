//! Provider-owned config schema for the `chatgpt` source (Program A goal
//! #1). Schema-only (serde + anyhow).

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "api",
        toml: include_str!("../examples/minimal_api.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "api",
        toml: include_str!("../examples/complete_api.toml"),
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatgptConfig {
    /// The options every source takes: an attachment size limit, when to
    /// give up on failing requests, and the debugging event tape.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which stored latchkey account this source signs in as.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// The live ChatGPT API — the one way in.
    #[serde(default)]
    pub api: Option<ChatgptApiSync>,
}

impl ChatgptConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatgptApiSync {
    /// Only sync conversations whose `update_time` is at or after this
    /// instant (RFC 3339 or `YYYY-MM-DD`, assumed UTC). Older
    /// conversations are never detail-fetched, and the listing walk
    /// stops early once it pages past the cutoff (the listing is
    /// newest-updated-first). Moving the date further back later
    /// backfills on the next run. Unset syncs everything.
    #[serde(default)]
    pub since: Option<String>,
    /// Stop listing after this many pages of 100 conversations. Unset
    /// lists them all.
    #[serde(default)]
    pub max_pages: Option<i64>,
    /// Fetch at most this many conversations per run. Unset fetches every
    /// one that is owed.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Seconds to wait between requests. A sync does not use it yet.
    #[serde(default)]
    pub sleep_between: Option<f64>,
    /// When non-empty, restrict the fetch to exactly these conversation ids
    /// (bare id or a paste-able `https://chatgpt.com/c/<id>` URL).
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type ChatgptRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for ChatgptConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}
