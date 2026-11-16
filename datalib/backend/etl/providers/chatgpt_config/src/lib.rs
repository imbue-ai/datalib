//! Provider-owned config schema for the `chatgpt` source (Program A goal
//! #1). Schema-only (serde + anyhow).

pub use datalib_source_common::WhenDeletedUpstream;
use datalib_source_common::{
    not_negative, seconds_between_requests, LatchkeySettings, LocalPath, SourceCommon,
};
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
/// (`IngestMethods` below). With both, the export fills in what the store
/// has never held, then the API run decides the rest, and only its
/// listing prunes.
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
    /// `api`, it seeds what the store has never held and prunes nothing.
    /// Either way it is read again only once its bytes change.
    #[serde(default)]
    pub export: Option<LocalPath>,
    /// What becomes of the copy of a conversation (or anything else this
    /// source holds) once it is deleted on chatgpt.com, or a newer export
    /// leaves it out: `"delete_local_copy"` (the default) deletes it here
    /// too; `"keep_local_copy"` keeps it as last fetched, so a person can
    /// delete it upstream for privacy and still have it.
    #[serde(default)]
    pub when_deleted_upstream: WhenDeletedUpstream,
}

impl ChatgptConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            datalib_time::TimeWindow::check("api", api.since.as_deref(), None)
                .map_err(anyhow::Error::msg)?;
            not_negative("api.max_listing_pages", api.max_listing_pages)
                .map_err(anyhow::Error::msg)?;
            not_negative(
                "api.max_conversations_per_run",
                api.max_conversations_per_run,
            )
            .map_err(anyhow::Error::msg)?;
            not_negative(
                "api.refetch_newest_conversations",
                api.refetch_newest_conversations,
            )
            .map_err(anyhow::Error::msg)?;
            seconds_between_requests("api.seconds_between_requests", api.seconds_between_requests)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

/// The API's settings. `since`, `max_conversations_per_run`,
/// `refetch_newest_conversations`, `seconds_between_requests` and
/// `conv_uuids` mean the same on the `claude` source.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatgptApiSync {
    /// Only sync conversations whose `update_time` is at or after this:
    /// a day (`YYYY-MM-DD`, the run's own midnight), an RFC 3339 instant, or an
    /// offset from the run's start (`-30d`). Older conversations are
    /// never detail-fetched, and the listing walk stops early once it
    /// pages past the cutoff (the listing is newest-updated-first). One
    /// already mirrored stays. Moving the start further back later
    /// backfills on the next run. Unset syncs everything. Ignored, with a
    /// warning, beside `conv_uuids`.
    #[serde(default)]
    pub since: Option<String>,
    /// Stop listing after this many pages of 100 conversations. Unset
    /// lists them all. A listing cut short deletes nothing here. Ignored,
    /// with a warning, beside `conv_uuids`.
    #[serde(default)]
    pub max_listing_pages: Option<i64>,
    /// Fetch at most this many conversations per run, the ones never
    /// fetched first; the next run fetches the rest. Unset fetches every
    /// one that is owed. Ignored, with a warning, beside `conv_uuids`.
    #[serde(default)]
    pub max_conversations_per_run: Option<i64>,
    /// Fetch this many of the most recently updated conversations again
    /// every run, even when the copy here is already at the update time
    /// listed: a check against the live copy. Unset fetches only what
    /// changed. Ignored, with a warning, beside `conv_uuids`.
    #[serde(default)]
    pub refetch_newest_conversations: Option<i64>,
    /// Seconds to wait between requests, from 0 to 3600. Unset waits
    /// none; the download still backs off on its own when rate-limited.
    #[serde(default)]
    pub seconds_between_requests: Option<f64>,
    /// When non-empty, fetch exactly these conversations every run, by id
    /// or a paste-able `https://chatgpt.com/c/<id>` URL, with no listing.
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}

impl ChatgptApiSync {
    /// The wait between requests; `validate` has refused a value this
    /// would not fit.
    pub fn wait_between_requests(&self) -> std::time::Duration {
        seconds_between_requests(
            "api.seconds_between_requests",
            self.seconds_between_requests,
        )
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

    /// The wait was once read and then ignored: a sync hard-coded none.
    /// A negative one would have panicked `Duration` once read.
    #[test]
    fn seconds_between_requests_is_read_and_held_to_seconds() {
        let api = |s: Option<f64>| ChatgptApiSync {
            seconds_between_requests: s,
            ..Default::default()
        };
        assert_eq!(
            api(Some(0.5)).wait_between_requests(),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(api(None).wait_between_requests(), std::time::Duration::ZERO);
        for bad in [-1.0, f64::NAN, 1e9] {
            let cfg = ChatgptConfig {
                api: Some(api(Some(bad))),
                ..Default::default()
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("api.seconds_between_requests"), "{err}");
        }
    }

    /// An export beside `api` seeds what the API keeps fresh, so a step
    /// may name both.
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
        for key in [
            "max_listing_pages",
            "max_conversations_per_run",
            "refetch_newest_conversations",
        ] {
            let cfg: ChatgptConfig = toml::from_str(&format!("[api]\n{key} = -3\n")).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains(&format!("`api.{key}`")), "{err}");
        }
    }

    /// The names these settings had before they matched `claude`'s are
    /// refused at load; `datalib-migrate-config` rewrites them.
    #[test]
    fn the_old_names_are_refused() {
        for old in ["max_pages = 1", "limit = 1", "sleep_between = 1.0"] {
            assert!(toml::from_str::<ChatgptConfig>(&format!("[api]\n{old}\n")).is_err());
        }
    }

    /// `api.since` takes the shared window grammar, and a typo is
    /// refused at load with the key named; `until` is not a key here.
    #[test]
    fn since_takes_the_window_grammar() {
        let load = |since: &str| {
            toml::from_str::<ChatgptConfig>(&format!("[api]\nsince = {since:?}\n"))
                .unwrap()
                .validate()
        };
        for ok in ["2024-03-20", "2024-03-20T18:28:51+02:00", "-30d"] {
            load(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let err = load("last month").unwrap_err().to_string();
        assert!(err.contains("`api.since`"), "{err}");
        assert!(toml::from_str::<ChatgptConfig>("[api]\nuntil = \"+1d\"\n").is_err());
    }
}
