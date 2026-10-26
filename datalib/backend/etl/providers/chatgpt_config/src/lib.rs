//! Provider-owned config schema for the `chatgpt` source (Program A goal
//! #1). Schema-only (serde + anyhow).

use datalib_source_common::{not_negative, LatchkeySettings, LocalPath, SourceCommon};
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

/// Two ways in, one raw store: `api` walks chatgpt.com, `export` reads
/// a data export off disk. An `ingest` step with neither is refused
/// (`IngestMethods` below). With both, the API run takes from the export
/// what it holds at the version listed, and only the listing prunes.
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
    /// The live ChatGPT API.
    #[serde(default)]
    pub api: Option<ChatgptApiSync>,
    /// A ChatGPT data export: the `.zip` as it downloaded, or the folder
    /// it was unzipped into. Alone, it is ingested as a whole snapshot:
    /// what the export no longer has is pruned from the store. Beside
    /// `api`, it is a local copy the API asks first, and prunes nothing.
    #[serde(default)]
    pub export: Option<LocalPath>,
}

impl ChatgptConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            not_negative("api.max_pages", api.max_pages).map_err(anyhow::Error::msg)?;
            not_negative("api.limit", api.limit).map_err(anyhow::Error::msg)?;
            if let Some(s) = api.sleep_between {
                anyhow::ensure!(
                    s.is_finite() && (0.0..=3600.0).contains(&s),
                    "`api.sleep_between` is {s}; it is seconds between requests, from 0 to 3600"
                );
            }
        }
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
    /// Seconds to wait between requests, from 0 to 3600. Unset waits
    /// none; the download still backs off on its own when rate-limited.
    #[serde(default)]
    pub sleep_between: Option<f64>,
    /// When non-empty, restrict the fetch to exactly these conversation ids
    /// (bare id or a paste-able `https://chatgpt.com/c/<id>` URL).
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}

impl ChatgptApiSync {
    /// The wait between requests; `validate` has refused a value this
    /// would not fit.
    pub fn sleep_between(&self) -> std::time::Duration {
        self.sleep_between
            .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
            .unwrap_or_default()
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type ChatgptRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for ChatgptConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("api"),
        datalib_source_common::IngestMethod::local("export"),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sleep_between` was read and then ignored: a sync hard-coded no
    /// wait. A negative one would have panicked `Duration` once read.
    #[test]
    fn sleep_between_is_read_and_held_to_seconds() {
        let api = |s: Option<f64>| ChatgptApiSync {
            sleep_between: s,
            ..Default::default()
        };
        assert_eq!(
            api(Some(0.5)).sleep_between(),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(api(None).sleep_between(), std::time::Duration::ZERO);
        for bad in [-1.0, f64::NAN, 1e9] {
            let cfg = ChatgptConfig {
                api: Some(api(Some(bad))),
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("api.sleep_between"), "{err}");
        }
    }

    /// An export beside `api` is the copy the API asks first, so a
    /// step may name both.
    #[test]
    fn api_and_export_together_are_one_step() {
        let c: ChatgptConfig =
            toml::from_str("api = {}\n[export]\npath = \"~/chatgpt-export\"\n").unwrap();
        c.validate().unwrap();
        assert!(c.api.is_some() && c.export.is_some());
        let export_only: ChatgptConfig =
            toml::from_str("[export]\npath = \"~/chatgpt-export\"\n").unwrap();
        export_only.validate().unwrap();
        assert!(export_only
            .export
            .unwrap()
            .path()
            .ends_with("chatgpt-export"));
    }

    #[test]
    fn negative_counts_are_refused_by_name() {
        let cfg = ChatgptConfig {
            api: Some(ChatgptApiSync {
                limit: Some(-3),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("api.limit"));
    }
}
