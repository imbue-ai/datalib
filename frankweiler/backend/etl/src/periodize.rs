//! Time-bucketing for chat-shaped translate steps.
//!
//! Chat providers (beeper, signal, the upcoming whatsapp and googlechat
//! readers) all face the same problem: a long-lived conversation has
//! tens of thousands of messages, and rendering it into a single
//! markdown file makes every new message re-fingerprint the whole
//! transcript — and turns the search-grid preview pane into a slow
//! many-MB scroll. The fix every provider lands on is the same: split
//! each chat into period-keyed buckets (`2024-03`, `2024-04`, …) and
//! render one `.md` per bucket.
//!
//! This module owns the `Period` knob + its key derivation so all the
//! providers agree on the same period_key strings, and a single CLI
//! config schema works across them.
//!
//! Two derivation paths:
//!
//! * [`Period::strftime_fmt`] — for providers that bucket in SQL
//!   (`strftime(<fmt>, ts/1000, 'unixepoch')` in a GROUP BY). Beeper
//!   pre-buckets in SQLite this way.
//! * [`Period::key_for_ms`] — for providers that bucket in Rust after
//!   pulling rows out (signal decodes prost payloads, so the
//!   bucketing can't happen in SQL).
//!
//! Both paths produce the same `period_key` strings, so a sidecar
//! emitted by either provider lines up with the other.

use anyhow::{bail, Result};
use chrono::{Datelike, TimeZone, Utc};

/// How many messages share one rendered markdown bucket.
///
/// The default a config should fall back to is [`Period::Month`] —
/// matches what beeper has been shipping and is the right point on
/// the granularity / file-count tradeoff for typical chat volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Month,
    Day,
    Year,
    All,
}

impl Period {
    /// Parse a YAML config knob. `None` yields the default (`Month`),
    /// so providers can pass `sync.period` through directly.
    pub fn from_config(s: Option<&str>) -> Result<Self> {
        Ok(match s.unwrap_or("month").to_ascii_lowercase().as_str() {
            "month" => Period::Month,
            "day" => Period::Day,
            "year" => Period::Year,
            "all" => Period::All,
            other => bail!("unknown period {other:?}; expected one of: month, day, year, all"),
        })
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
}
