//! Config schema for the `garmin` source: a Garmin Connect account,
//! mirrored over the same API the Connect phone app uses. Schema only
//! (serde + anyhow), so anything that needs to understand a config can
//! link this without the downloader. `api` is its one way in.

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Where the per-day walk starts when the config names no `since`.
/// A year covers what a first-time user usually wants to see, and the
/// value is recorded in the store, so widening it later backfills.
pub const DEFAULT_SINCE_DAYS: i64 = 365;

/// How many days before each metric's cursor a run re-fetches. A watch
/// syncs when it feels like it, and Garmin recomputes sleep, HRV and
/// training status for a day after that day ends, so the trailing week
/// is re-read rather than trusted.
pub const DEFAULT_REFRESH_DAYS: i64 = 7;

/// The garmin-owned slice of a `garmin` source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GarminConfig {
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub api: Option<GarminApi>,
}

/// The live-API method. The credential is not here: it is the OAuth1
/// token `datalib-step login garmin` (or garth) writes under
/// `token_dir`, which outlives any one config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GarminApi {
    /// Directory holding `oauth1_token.json` (and the cached
    /// `oauth2_token.json`), in garth's file format so a token minted
    /// by either tool works. Default `~/.garth`.
    #[serde(default)]
    pub token_dir: Option<String>,
    /// Earliest calendar day to mirror, `YYYY-MM-DD`. Default: a year
    /// before the first run. Move it earlier to backfill.
    #[serde(default)]
    pub since: Option<String>,
    /// Last calendar day to mirror, `YYYY-MM-DD`. Default: today. A day
    /// in the past fixes the window, so a mirror of a finished stretch
    /// stops growing — what a golden test wants.
    #[serde(default)]
    pub until: Option<String>,
    /// Days before each metric's cursor to re-fetch every run. Default 7.
    #[serde(default)]
    pub refresh_days: Option<i64>,
    /// Which per-day metrics to mirror. Default: every one the provider
    /// knows (`DAILY_METRICS`). Name a subset to trim the request count.
    #[serde(default)]
    pub metrics: Option<Vec<String>>,
    /// Fetch each activity's original FIT file into the blob store.
    /// Default true — the FIT file is the complete record of a workout;
    /// the JSON summary is a projection of it.
    #[serde(default)]
    pub activity_files: Option<bool>,
    /// Fetch each day's wellness FIT bundle (the zip Garmin serves at
    /// `/download-service/files/wellness/<date>`: all-day heart rate,
    /// stress, steps, body battery and sleep at sensor resolution).
    /// Default false: one zip per day, and the per-day JSON metrics
    /// already carry the same series at chart resolution.
    #[serde(default)]
    pub wellness_files: Option<bool>,
}

/// Every per-day metric the provider mirrors, in the order it walks
/// them. The names are the `metric` column of `garmin_daily` and the
/// values `metrics` accepts, so a rename here re-keys stored rows.
pub const DAILY_METRICS: &[&str] = &[
    "daily_summary",
    "heart_rate",
    "sleep",
    "stress",
    "body_battery",
    "body_battery_events",
    "respiration",
    "spo2",
    "hrv",
    "training_readiness",
    "training_status",
    "intensity_minutes",
    "floors",
    "hydration",
    "steps_chart",
    "fitness_age",
    "max_metrics",
    "daily_events",
    "endurance_score",
    "hill_score",
];

impl GarminConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let Some(api) = &self.api else {
            return Ok(());
        };
        if let Some(since) = &api.since {
            if !is_yyyy_mm_dd(since) {
                anyhow::bail!("garmin: api.since {since:?} is not YYYY-MM-DD");
            }
        }
        if let Some(until) = &api.until {
            if !is_yyyy_mm_dd(until) {
                anyhow::bail!("garmin: api.until {until:?} is not YYYY-MM-DD");
            }
            if api.since.as_ref().is_some_and(|since| since > until) {
                anyhow::bail!("garmin: api.until {until:?} is before api.since");
            }
        }
        if api.refresh_days.is_some_and(|d| d < 0) {
            anyhow::bail!("garmin: api.refresh_days must not be negative");
        }
        if let Some(metrics) = &api.metrics {
            for m in metrics {
                if !DAILY_METRICS.contains(&m.as_str()) {
                    anyhow::bail!(
                        "garmin: api.metrics names {m:?}, which is not one of: {}",
                        DAILY_METRICS.join(", ")
                    );
                }
            }
        }
        Ok(())
    }
}

impl GarminApi {
    pub fn refresh_days(&self) -> i64 {
        self.refresh_days.unwrap_or(DEFAULT_REFRESH_DAYS)
    }

    pub fn activity_files(&self) -> bool {
        self.activity_files.unwrap_or(true)
    }

    pub fn wellness_files(&self) -> bool {
        self.wellness_files.unwrap_or(false)
    }

    /// The metrics this config asks for, in walk order.
    pub fn metrics(&self) -> Vec<&str> {
        match &self.metrics {
            Some(list) => DAILY_METRICS
                .iter()
                .copied()
                .filter(|m| list.iter().any(|l| l == m))
                .collect(),
            None => DAILY_METRICS.to_vec(),
        }
    }
}

fn is_yyyy_mm_dd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// Params for the render step: no provider-specific knobs.
pub type GarminRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GarminConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(api: GarminApi) -> GarminConfig {
        GarminConfig {
            common: Default::default(),
            api: Some(api),
        }
    }

    #[test]
    fn empty_api_is_valid_and_asks_for_everything() {
        let c = cfg(GarminApi::default());
        assert!(c.validate().is_ok());
        let api = c.api.unwrap();
        assert_eq!(api.metrics(), DAILY_METRICS.to_vec());
        assert!(api.activity_files());
        assert!(!api.wellness_files());
        assert_eq!(api.refresh_days(), DEFAULT_REFRESH_DAYS);
    }

    #[test]
    fn until_must_be_a_date_no_earlier_than_since() {
        let window = |since: &str, until: &str| {
            cfg(GarminApi {
                since: Some(since.into()),
                until: Some(until.into()),
                ..Default::default()
            })
            .validate()
        };
        assert!(window("2025-08-01", "2025-08-31").is_ok());
        assert!(window("2025-08-01", "2025-08-01").is_ok());
        assert!(window("2025-08-01", "2025-07-31").is_err());
        assert!(window("2025-08-01", "31 Aug").is_err());
    }

    #[test]
    fn since_must_be_a_date() {
        let c = cfg(GarminApi {
            since: Some("2026/01/01".into()),
            ..Default::default()
        });
        assert!(c.validate().is_err());
    }

    #[test]
    fn metrics_subset_keeps_walk_order_and_rejects_unknowns() {
        let c = cfg(GarminApi {
            metrics: Some(vec!["sleep".into(), "daily_summary".into()]),
            ..Default::default()
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.api.unwrap().metrics(), vec!["daily_summary", "sleep"]);
        let bad = cfg(GarminApi {
            metrics: Some(vec!["golf".into()]),
            ..Default::default()
        });
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("golf"), "{err}");
    }
}
