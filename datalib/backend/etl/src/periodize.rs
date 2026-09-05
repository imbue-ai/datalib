//! Time-bucketing for chat-shaped translate steps.

use anyhow::{bail, Result};
use chrono::{Datelike, TimeZone, Utc};

/// How many messages share one rendered markdown bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Month,
    Day,
    Year,
    All,
}

impl Period {
    pub fn from_config(s: Option<&str>) -> Result<Self> {
        Ok(match s.unwrap_or("month").to_ascii_lowercase().as_str() {
            "month" => Period::Month,
            "day" => Period::Day,
            "year" => Period::Year,
            "all" => Period::All,
            other => bail!("unknown period {other:?}; expected one of: month, day, year, all"),
        })
    }

    /// The config spelling this variant round-trips from
    /// [`Self::from_config`]. Stable across releases — it's recorded in
    /// render cursors (see
    /// [`crate::render_cursor::read_for_params`]), so changing a string
    /// here would read as a config change and re-render every tree.
    pub fn as_config_str(self) -> &'static str {
        match self {
            Period::Month => "month",
            Period::Day => "day",
            Period::Year => "year",
            Period::All => "all",
        }
    }

    /// SQLite format string passed to `strftime(<fmt>, ts/1000,
    /// 'unixepoch')`. `All` returns a value that won't be used in a
    /// real GROUP BY (callers detect All and substitute
    /// `key_for_all()` as a literal column), but is still a valid
    /// format so a misuse path doesn't crash the renderer.
    pub fn strftime_fmt(self) -> &'static str {
        match self {
            Period::Month => "%Y-%m",
            Period::Day => "%Y-%m-%d",
            Period::Year => "%Y",
            Period::All => "%Y-%m-%dT%H:%M:%S",
        }
    }

    /// Literal sentinel used as the `period_key` of the single bucket
    /// that holds every event when `Period::All` is selected. Callers
    /// (Rust and SQL) both substitute this directly rather than
    /// reading anything off a timestamp.
    pub const fn key_for_all() -> &'static str {
        "all"
    }

    pub fn key_for_undated(self) -> String {
        self.key_for_ms(0)
    }

    /// Compute the period_key for a unix-epoch millisecond timestamp.
    /// Mirrors `strftime_fmt` for Rust-side bucketing — produces the
    /// same `2024-03` / `2024-03-15` / `2024` keys SQL would produce.
    /// `Period::All` short-circuits to `key_for_all()`.
    pub fn key_for_ms(self, ts_ms: i64) -> String {
        if matches!(self, Period::All) {
            return Self::key_for_all().to_string();
        }
        // saturating_div for safety against MIN_VALUE; ms→s.
        let secs = ts_ms.div_euclid(1000);
        let dt = match Utc.timestamp_opt(secs, 0).single() {
            Some(d) => d,
            None => return "1970-01-01".to_string(),
        };
        match self {
            Period::Year => format!("{:04}", dt.year()),
            Period::Month => format!("{:04}-{:02}", dt.year(), dt.month()),
            Period::Day => format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
            Period::All => unreachable!("handled above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_defaults_to_month() {
        assert_eq!(Period::from_config(None).unwrap(), Period::Month);
    }

    #[test]
    fn from_config_rejects_unknown() {
        assert!(Period::from_config(Some("decade")).is_err());
    }

    #[test]
    fn from_config_is_case_insensitive() {
        assert_eq!(Period::from_config(Some("DAY")).unwrap(), Period::Day);
        assert_eq!(Period::from_config(Some("All")).unwrap(), Period::All);
    }

    #[test]
    fn key_for_ms_month() {
        // 2024-03-15T12:34:56Z → 1710505896 → 1710505896000
        assert_eq!(Period::Month.key_for_ms(1_710_505_896_000), "2024-03");
    }

    #[test]
    fn key_for_ms_day_year() {
        assert_eq!(Period::Day.key_for_ms(1_710_505_896_000), "2024-03-15");
        assert_eq!(Period::Year.key_for_ms(1_710_505_896_000), "2024");
    }

    #[test]
    fn key_for_ms_all_short_circuits() {
        assert_eq!(Period::All.key_for_ms(1_710_505_896_000), "all");
        assert_eq!(Period::All.key_for_ms(0), "all");
    }

    #[test]
    fn key_for_ms_handles_negative_timestamps() {
        // Pre-1970 timestamp shouldn't panic.
        let k = Period::Month.key_for_ms(-1_000);
        assert!(k.starts_with("1969") || k == "1970-01");
    }

    #[test]
    fn key_for_undated_matches_the_epoch_bucket() {
        // Pinned deliberately: these keys are inputs to every provider's
        // `markdown_uuid`, so changing them silently re-keys documents.
        assert_eq!(Period::Month.key_for_undated(), "1970-01");
        assert_eq!(Period::Day.key_for_undated(), "1970-01-01");
        assert_eq!(Period::Year.key_for_undated(), "1970");
        assert_eq!(Period::All.key_for_undated(), "all");
    }

    #[test]
    fn as_config_str_round_trips_every_variant() {
        // The spelling is recorded in render cursors, so a typo in one
        // arm would silently invalidate every signal render tree in the
        // field on the next release.
        for p in [Period::Month, Period::Day, Period::Year, Period::All] {
            assert_eq!(
                Period::from_config(Some(p.as_config_str())).unwrap(),
                p,
                "{:?} did not round-trip through {:?}",
                p,
                p.as_config_str()
            );
        }
    }
}
