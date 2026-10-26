//! Config schema for the `zoom` source: one Zoom user's meetings, their
//! cloud recordings' transcripts, who attended and the AI Companion's
//! summaries. Schema only (serde + anyhow), so anything that needs to
//! understand a config can link this without the downloader. `api` is
//! its one way in.

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

/// The first day a sync asks Zoom about when `since` is not set: Zoom
/// opened to the public in January 2013, so nothing is older.
pub const EARLIEST_SINCE: &str = "2013-01-01";

/// How many of the newest days every run lists again when
/// `refresh_days` is not set.
pub const DEFAULT_REFRESH_DAYS: u32 = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZoomConfig {
    /// `common.blob_size_limit_bytes` caps the video and audio files
    /// `api.media` fetches; the transcripts and chat files are never
    /// held back by it.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which stored latchkey account of the `zoom` service this source
    /// signs in as.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Zoom's REST API, the one way in.
    #[serde(default)]
    pub api: Option<ZoomApi>,
}

impl ZoomConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            api.validate()?;
        }
        Ok(())
    }
}

/// Whose meetings, how far back, and which parts of each.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZoomApi {
    /// The Zoom user whose meetings to copy, by email address or Zoom
    /// user id. The sign-in latchkey makes is for the whole Zoom
    /// account, not for one person in it, so this says whose. Unset: the
    /// user the sign-in belongs to where Zoom says, else the account's
    /// only user; an account with several users has to name one.
    #[serde(default)]
    pub user: Option<String>,
    /// The first day to ask Zoom about, `YYYY-MM-DD`. Default 2013-01-01,
    /// so the first sync finds every recording Zoom still keeps. Zoom
    /// lists recordings a month at a time, so this costs one request a
    /// month, once.
    #[serde(default)]
    pub since: Option<String>,
    /// How many of the newest days every sync lists again, since a
    /// transcript or a summary is added hours after its meeting ends and
    /// a recording can be deleted. Default 30.
    #[serde(default)]
    pub refresh_days: Option<i64>,
    /// Who attended each meeting, and the meetings that were not
    /// recorded, from Zoom's reports. Zoom keeps who attended for 15
    /// months, and its reports reach back six. Needs a paid account.
    /// Default true.
    #[serde(default = "yes")]
    pub participants: bool,
    /// The AI Companion's summary of each meeting it summarized.
    /// Default true.
    #[serde(default = "yes")]
    pub summaries: bool,
    /// The video and audio files of each recording too, each up to
    /// `common.blob_size_limit_bytes`. An hour of video is several
    /// hundred MB. Default false: only the transcript, the chat and the
    /// other small files.
    #[serde(default)]
    pub media: bool,
}

fn yes() -> bool {
    true
}

impl Default for ZoomApi {
    fn default() -> Self {
        Self {
            user: None,
            since: None,
            refresh_days: None,
            participants: true,
            summaries: true,
            media: false,
        }
    }
}

impl ZoomApi {
    pub fn validate(&self) -> anyhow::Result<()> {
        datalib_source_common::not_negative("api.refresh_days", self.refresh_days)
            .map_err(anyhow::Error::msg)?;
        if let Some(since) = &self.since {
            if !is_iso_date(since) {
                anyhow::bail!("zoom: `api.since` is a date, `YYYY-MM-DD`; got {since:?}");
            }
        }
        if self.user.as_ref().is_some_and(|u| u.trim().is_empty()) {
            anyhow::bail!(
                "zoom: `api.user` names a Zoom user by email or id; omit it to copy the \
                 sign-in's own user"
            );
        }
        Ok(())
    }

    pub fn since(&self) -> &str {
        self.since.as_deref().unwrap_or(EARLIEST_SINCE)
    }

    pub fn refresh_days(&self) -> u32 {
        self.refresh_days
            .map_or(DEFAULT_REFRESH_DAYS, |d| d.clamp(0, u32::MAX as i64) as u32)
    }
}

/// `YYYY-MM-DD` by its shape; the download parses it as a date.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// No render knobs.
pub type ZoomRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for ZoomConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_api_table_takes_every_default() {
        let cfg: ZoomConfig = serde_json::from_str(r#"{"api":{}}"#).unwrap();
        cfg.validate().unwrap();
        let api = cfg.api.unwrap();
        assert_eq!(api.user, None);
        assert_eq!(api.since(), EARLIEST_SINCE);
        assert_eq!(api.refresh_days(), DEFAULT_REFRESH_DAYS);
        assert!(api.participants && api.summaries && !api.media);
    }

    #[test]
    fn a_since_that_is_not_a_date_is_refused() {
        let cfg: ZoomConfig = serde_json::from_str(r#"{"api":{"since":"last year"}}"#).unwrap();
        assert!(cfg.validate().is_err());
        let cfg: ZoomConfig = serde_json::from_str(r#"{"api":{"since":"2364-03-02"}}"#).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn a_blank_user_or_a_negative_window_is_refused() {
        for bad in [r#"{"api":{"user":" "}}"#, r#"{"api":{"refresh_days":-1}}"#] {
            let cfg: ZoomConfig = serde_json::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<ZoomConfig>(r#"{"api":{"channels":["x"]}}"#).is_err());
    }
}
