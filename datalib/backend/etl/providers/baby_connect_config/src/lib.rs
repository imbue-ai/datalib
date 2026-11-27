//! Provider-owned config schema for the `baby_connect` source.
//! Schema-only (serde + anyhow), so `datalib-step` can name
//! `BabyConnectConfig` without linking the provider.

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
pub struct BabyConnectConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which stored latchkey account this source signs in as.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// The Baby Connect web app at app.babyconnect.com, the one way in.
    #[serde(default)]
    pub api: Option<BabyConnectSync>,
}

impl BabyConnectConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            datalib_time::TimeWindow::check("api", api.since.as_deref(), api.until.as_deref())
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

/// Which days to copy. Baby Connect answers one day at a time, so the
/// first run asks once for every day from the eldest child's birthday to
/// today; later runs ask only for the days not yet read, and the last
/// two again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BabyConnectSync {
    /// The first day to read: a day (`2031-03-01`), an RFC 3339 instant,
    /// or an offset from the run's start (`-90d`). Omit it to start at
    /// the eldest child's birthday.
    #[serde(default)]
    pub since: Option<String>,
    /// The last day to read, in the same forms. Omit it to read up to
    /// today; set it to stop at the day you stopped using the app.
    #[serde(default)]
    pub until: Option<String>,
}

/// Params for the render step, which makes one document per child
/// (`datalib_etl_baby_connect_render`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BabyConnectRenderConfig {
    #[serde(default)]
    pub common: datalib_source_common::RenderCommon,
    /// The IANA zone the entries were logged in (`America/Los_Angeles`).
    /// Baby Connect keeps an entry's time as a clock reading with no
    /// zone, so this is what makes it a moment; daylight saving is
    /// applied for each date. Unset reads the times as UTC, and says so
    /// on the source's Manage row.
    #[serde(default)]
    pub time_zone: Option<String>,
}

impl BabyConnectRenderConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(name) = &self.time_zone {
            datalib_time::NamedZone::get(name)
                .ok_or_else(|| anyhow::anyhow!("`time_zone` {name:?} is not a time zone name"))?;
        }
        Ok(())
    }
}

impl datalib_source_common::IngestMethods for BabyConnectConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_api_table_reads_every_day() {
        let cfg: BabyConnectConfig = serde_json::from_str(r#"{"api":{}}"#).unwrap();
        assert!(cfg.validate().is_ok());
        let api = cfg.api.unwrap();
        assert_eq!(api.since, None);
        assert_eq!(api.until, None);
    }

    /// `since` and `until` take the window grammar every source shares,
    /// so a typo is refused at load naming the key.
    #[test]
    fn the_window_takes_the_shared_grammar() {
        let window = |since: &str, until: &str| {
            let cfg: BabyConnectConfig = serde_json::from_str(&format!(
                r#"{{"api":{{"since":"{since}","until":"{until}"}}}}"#
            ))
            .unwrap();
            cfg.validate()
        };
        window("2368-01-01", "2370-12-31").unwrap();
        window("-90d", "-1d").unwrap();
        let e = window("stardate 45000", "2370-12-31")
            .unwrap_err()
            .to_string();
        assert!(e.contains("`api.since`"), "{e}");
    }

    #[test]
    fn the_render_takes_a_named_zone() {
        let ok: BabyConnectRenderConfig =
            serde_json::from_str(r#"{"time_zone":"America/Los_Angeles"}"#).unwrap();
        ok.validate().unwrap();
        let bad: BabyConnectRenderConfig =
            serde_json::from_str(r#"{"time_zone":"Bajor/Ashalla"}"#).unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<BabyConnectConfig>(r#"{"api":{"kids":["x"]}}"#).is_err());
    }
}
