//! Timestamp utilities for the frankweiler workspace.
//!
//! Every `now()` and every inbound-timestamp parse funnels through this
//! crate. The point is to land two architectural rules in exactly one
//! place each, instead of re-litigating them at every callsite:
//!
//! 1. **Generated timestamps carry the generating system's local-tz
//!    offset, not UTC.** A timestamp with offset is strictly more
//!    information than the same instant in UTC: you can recover UTC
//!    from `-07:00`, but you can't recover `-07:00` from `Z`. Useful
//!    for forensics ("where was this run?") and for showing the user
//!    their own local time without a separate "where was this
//!    generated" field.
//! 2. **We never fabricate values.** [`parse_strict`] requires the
//!    upstream string to carry an explicit offset.
//!    [`parse_with_assumed_utc`] is the **single function in the whole
//!    repo** where "assume UTC because upstream gave us no offset" is
//!    legal — and it should be used sparingly, only when we've audited
//!    the upstream and confirmed naive-means-UTC.
//!
//! See [`docs/dev/data_architecture_plan.md`](../../../../docs/dev/data_architecture_plan.md)
//! §P0.5 for the architectural backstory, and
//! [`docs/dev/data_architecture_ingestion.md`](../../../../docs/dev/data_architecture_ingestion.md)
//! for the "no fabricated values" principle.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// An RFC 3339 timestamp that carries an **explicit** UTC offset.
///
/// Constructable only through the helpers in this crate — there is no
/// public `new(DateTime<FixedOffset>)`. Callers that want a "now"
/// stamp call [`now_local`]; callers parsing upstream strings go
/// through [`parse_strict`] or (rarely) [`parse_with_assumed_utc`].
///
/// Serializes as the RFC 3339 string (so it round-trips through JSON
/// and through `sqlx` `TEXT` columns cleanly).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IsoOffsetTimestamp(DateTime<FixedOffset>);

impl IsoOffsetTimestamp {
    /// Wall-clock now in the **local** timezone. The canonical "now"
    /// for stamping `fetched_at`, `created_at`, run-start markers, and
    /// the like. See module docs for why local-offset beats UTC.
    pub fn now_local() -> Self {
        Self(Local::now().fixed_offset())
    }

    /// Convert a Unix epoch-millisecond value (typical of chat
    /// upstreams: Signal, Beeper, Slack `ts`) into an offsetted
    /// timestamp. Returns `None` for absurdly out-of-range values that
    /// chrono can't represent.
    ///
    /// The result carries `+00:00` because Unix epoch values are
    /// upstream-stamped in UTC by definition; nothing the local
    /// system knows about offset is relevant to interpreting them.
    pub fn from_unix_millis(ms: i64) -> Option<Self> {
        DateTime::<Utc>::from_timestamp_millis(ms).map(|dt| Self(dt.fixed_offset()))
    }

    /// Construct from a Unix epoch-second value. Same offset rules as
    /// [`from_unix_millis`].
    pub fn from_unix_seconds(s: i64) -> Option<Self> {
        DateTime::<Utc>::from_timestamp(s, 0).map(|dt| Self(dt.fixed_offset()))
    }

    /// Bump this timestamp forward by `n` microseconds. The canonical
    /// recipe for synthesizing sub-item stamps when upstream gave the
    /// parent a time but didn't give one to each child (anthropic /
    /// chatgpt blocks within a message, etc.). Keeps within-parent
    /// ordering stable. `n` can be negative.
    pub fn bump_micros(&self, n: i64) -> Self {
        Self(self.0 + chrono::Duration::microseconds(n))
    }

    /// Render as RFC 3339 with auto-selected sub-second precision and
    /// an explicit offset (never bare `Z`).
    pub fn to_rfc3339(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::AutoSi, false)
    }

    /// Render as RFC 3339 with seconds-precision and explicit offset.
    /// Use for cursor-like values where sub-second precision is noise.
    pub fn to_rfc3339_secs(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    /// Render as RFC 3339 with microsecond-precision and explicit offset.
    pub fn to_rfc3339_micros(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Micros, false)
    }

    /// Render as RFC 3339 with millisecond-precision and explicit offset.
    pub fn to_rfc3339_millis(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Millis, false)
    }

    /// Borrow the underlying `chrono` type. Use sparingly — preferring
    /// crate methods keeps the policy enforceable.
    pub fn inner(&self) -> DateTime<FixedOffset> {
        self.0
    }
}

impl fmt::Display for IsoOffsetTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl From<DateTime<FixedOffset>> for IsoOffsetTimestamp {
    fn from(dt: DateTime<FixedOffset>) -> Self {
        Self(dt)
    }
}

impl From<DateTime<Utc>> for IsoOffsetTimestamp {
    fn from(dt: DateTime<Utc>) -> Self {
        Self(dt.fixed_offset())
    }
}

impl Serialize for IsoOffsetTimestamp {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for IsoOffsetTimestamp {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_strict(&s).map_err(serde::de::Error::custom)
    }
}

impl FromStr for IsoOffsetTimestamp {
    type Err = TimestampParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_strict(s)
    }
}

/// Error returned by the parse helpers.
#[derive(Debug, thiserror::Error)]
pub enum TimestampParseError {
    #[error("timestamp {input:?} has no offset; explicit offset required")]
    MissingOffset { input: String },
    #[error("invalid RFC 3339 / ISO 8601 timestamp {input:?}: {source}")]
    Invalid {
        input: String,
        #[source]
        source: chrono::ParseError,
    },
}

/// Parse an RFC 3339 / ISO 8601 string that **already carries an
/// explicit offset** (e.g. `2026-06-10T14:23:00-07:00`,
/// `2026-06-10T21:23:00+00:00`, or `2026-06-10T21:23:00Z`).
///
/// This is the right helper for parsing values you've just written
/// yourself or that upstream guarantees to deliver with an offset.
pub fn parse_strict(s: &str) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    DateTime::parse_from_rfc3339(s)
        .map(IsoOffsetTimestamp)
        .map_err(|source| TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        })
}

/// Parse a timestamp that **might** have an explicit offset; if not,
/// assume UTC.
///
/// This is the **only** place in the repo where "assume UTC" is
/// allowed. Use it only for upstream feeds we've audited and confirmed
/// naive-means-UTC (some flawed exports, some older APIs). Any other
/// fallback path — local time, midnight, run start — is wrong.
///
/// Accepts:
/// - explicit-offset RFC 3339 (`...+00:00`, `...-07:00`, `...Z`)
/// - naive ISO 8601 with seconds (`2026-06-10T21:23:00`) → assumed UTC
/// - naive ISO 8601 with sub-seconds (`2026-06-10T21:23:00.123`) → assumed UTC
pub fn parse_with_assumed_utc(s: &str) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(IsoOffsetTimestamp(dt));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|source| TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        })?;
    let utc = Utc.from_utc_datetime(&naive);
    Ok(IsoOffsetTimestamp(utc.fixed_offset()))
}

/// Parse a string with an arbitrary `chrono` strftime format. The
/// format **must** include `%z` / `%:z` / `%#z` so the result carries
/// an explicit offset — that's the contract we enforce on every
/// parsed timestamp in the workspace.
///
/// Use this only for upstream feeds that ship a non-RFC 3339 shape
/// (e.g. yolink's CSV: `"2026/06/10 14:23:00-0700"`). For RFC 3339
/// inputs prefer [`parse_strict`].
pub fn parse_custom_strftime(
    s: &str,
    fmt: &str,
) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    debug_assert!(
        fmt.contains("%z") || fmt.contains("%:z") || fmt.contains("%#z"),
        "parse_custom_strftime format {fmt:?} lacks an offset spec — every parse path \
         must keep an explicit offset (use parse_with_assumed_utc for naive inputs)",
    );
    DateTime::parse_from_str(s, fmt)
        .map(IsoOffsetTimestamp)
        .map_err(|source| TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        })
}

/// Parse a bare `YYYY-MM-DD` date as **midnight UTC** of that day.
///
/// This is the one helper whose explicit purpose is to fabricate the
/// missing time-of-day + offset components. It exists for human-facing
/// CLI inputs (slack's `--since 2026-01-15`) where the user clearly
/// meant "the start of that day, somewhere reasonable" and the
/// alternative is rejecting friendly input. The fabrication is loud
/// in the name so reviewers see the cost.
///
/// Do **not** use this to translate upstream-provided values. Reach
/// for [`parse_strict`] (the upstream guaranteed an offset) or
/// [`parse_with_assumed_utc`] (the upstream lacks one and we've
/// audited what that means).
pub fn parse_yyyy_mm_dd_assumed_utc(s: &str) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    let naive = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|source| {
        TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        }
    })?;
    let ndt = naive
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is always a valid time-of-day");
    Ok(IsoOffsetTimestamp(
        Utc.from_utc_datetime(&ndt).fixed_offset(),
    ))
}

/// String-in/string-out shim for callsites that hold raw RFC 3339
/// strings (e.g. translate code carrying upstream timestamps through
/// to `GridRow.when_ts`). Tolerates `Z` (treated as `+00:00`). Returns
/// the bumped value rendered with microsecond precision and an
/// explicit offset. Returns `None` on parse failure so the caller can
/// pick its own fallback — *do not* silently swallow.
pub fn bump_micros_str(s: &str, n: i64) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let owned;
    let normalized: &str = if let Some(prefix) = s.strip_suffix('Z') {
        owned = format!("{prefix}+00:00");
        &owned
    } else {
        s
    };
    parse_strict(normalized)
        .ok()
        .map(|t| t.bump_micros(n).to_rfc3339_micros())
}

/// Cheap structural check that `s` is RFC 3339 with an explicit
/// offset. Useful in translate-time validators that want to assert
/// without keeping the parsed value.
pub fn validate_iso_offset(s: &str) -> Result<(), TimestampParseError> {
    parse_strict(s).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_local_has_explicit_offset() {
        let s = IsoOffsetTimestamp::now_local().to_rfc3339();
        // Last 6 chars must be like "+HH:MM" / "-HH:MM" — never "Z".
        let suffix = &s[s.len() - 6..];
        assert!(
            (suffix.starts_with('+') || suffix.starts_with('-')) && &suffix[3..4] == ":",
            "expected explicit offset suffix, got {s:?}"
        );
    }

    #[test]
    fn parse_strict_requires_offset() {
        assert!(parse_strict("2026-06-10T21:23:00Z").is_ok());
        assert!(parse_strict("2026-06-10T14:23:00-07:00").is_ok());
        assert!(matches!(
            parse_strict("2026-06-10T21:23:00"),
            Err(TimestampParseError::Invalid { .. })
        ));
    }

    #[test]
    fn assumed_utc_accepts_naive() {
        let t = parse_with_assumed_utc("2026-06-10T21:23:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-06-10T21:23:00+00:00");
        let t = parse_with_assumed_utc("2026-06-10T21:23:00.123456").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-06-10T21:23:00.123456+00:00");
    }

    #[test]
    fn assumed_utc_passes_offsetted_through() {
        let t = parse_with_assumed_utc("2026-06-10T14:23:00-07:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-06-10T14:23:00-07:00");
    }

    #[test]
    fn bump_micros_preserves_offset() {
        let t = parse_strict("2026-06-10T14:23:00-07:00").unwrap();
        assert_eq!(
            t.bump_micros(1).to_rfc3339(),
            "2026-06-10T14:23:00.000001-07:00"
        );
        assert_eq!(
            t.bump_micros(-1).to_rfc3339(),
            "2026-06-10T14:22:59.999999-07:00"
        );
    }

    #[test]
    fn serde_roundtrips_through_string() {
        let t = parse_strict("2026-06-10T14:23:00-07:00").unwrap();
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(j, "\"2026-06-10T14:23:00-07:00\"");
        let back: IsoOffsetTimestamp = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn from_unix_millis_round_trips_utc() {
        let t = IsoOffsetTimestamp::from_unix_millis(0).unwrap();
        assert_eq!(t.to_rfc3339_millis(), "1970-01-01T00:00:00.000+00:00");
        let t = IsoOffsetTimestamp::from_unix_millis(1_780_000_000_000).unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2026-05-28T20:26:40+00:00");
    }

    #[test]
    fn parse_custom_strftime_yolink_shape() {
        let t = parse_custom_strftime("2026/06/10 14:23:00-0700", "%Y/%m/%d %H:%M:%S%z").unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2026-06-10T14:23:00-07:00");
    }

    #[test]
    fn parse_yyyy_mm_dd_assumed_utc_explicit() {
        let t = parse_yyyy_mm_dd_assumed_utc("2026-01-15").unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2026-01-15T00:00:00+00:00");
        assert!(parse_yyyy_mm_dd_assumed_utc("not-a-date").is_err());
    }

    #[test]
    fn validate_iso_offset_rejects_naive() {
        assert!(validate_iso_offset("2026-06-10T14:23:00-07:00").is_ok());
        assert!(validate_iso_offset("2026-06-10T21:23:00").is_err());
    }
}
