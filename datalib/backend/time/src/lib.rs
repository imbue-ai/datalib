//! Timestamp utilities for the datalib workspace.
//!
//! Every `now()` and every inbound-timestamp parse funnels through here, so
//! two rules land in one place each instead of being re-litigated at every
//! callsite.
//!
//! **Generated timestamps carry the generating system's local offset, not
//! UTC.** An offset is strictly more information than the same instant in
//! UTC: you can recover UTC from `-07:00`, but not `-07:00` from `Z`.
//!
//! **A timestamp we cannot parse becomes a null, never a stand-in.** Every
//! parse helper returns a `Result`, and callers are expected to let the
//! failure become a `None`. An `unwrap_or(0)` on the way out of one of these
//! puts a real-looking `1970-01-01T00:00:00+00:00` into the grid, where it
//! sorts into a real position and answers `before:` / `after:` queries it
//! should not.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// An RFC 3339 timestamp that carries an **explicit** UTC offset.
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
    pub fn from_unix_millis(ms: i64) -> Option<Self> {
        DateTime::<Utc>::from_timestamp_millis(ms).map(|dt| Self(dt.fixed_offset()))
    }

    /// Bump this timestamp forward by `n` microseconds. The canonical
    /// recipe for synthesizing sub-item stamps when upstream gave the
    /// parent a time but didn't give one to each child (claude /
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

    pub fn to_rfc3339_secs(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    pub fn to_rfc3339_micros(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Micros, false)
    }

    pub fn to_rfc3339_millis(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Millis, false)
    }

    pub fn to_unix_millis(&self) -> i64 {
        self.0.timestamp_millis()
    }

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

pub fn parse_strict(s: &str) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    DateTime::parse_from_rfc3339(s)
        .map(IsoOffsetTimestamp)
        .map_err(|source| TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        })
}

/// Parse a timestamp that **might** have an explicit offset; if not, assume
/// UTC.
///
/// The **only** place in the repo where assuming UTC is allowed, and only for
/// upstream feeds we have audited and confirmed naive-means-UTC. Any other
/// fallback — local time, midnight, run start — is wrong.
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

/// Parse a timestamp in an arbitrary `chrono` strftime format that carries
/// **no offset**, assuming UTC.
///
/// Inherits [`parse_with_assumed_utc`]'s restriction: only for a feed we have
/// audited. It exists because some exports state their zone as a literal word
/// rather than a numeric offset (Google Chat's `"… AM UTC"`, LinkedIn's
/// `"2026-06-16 22:11:33 UTC"`), which chrono cannot turn into a
/// `FixedOffset`. The format must not contain an offset spec — if the input
/// can carry a real offset, use [`parse_custom_strftime`].
pub fn parse_custom_strftime_assumed_utc(
    s: &str,
    fmt: &str,
) -> Result<IsoOffsetTimestamp, TimestampParseError> {
    debug_assert!(
        !(fmt.contains("%z") || fmt.contains("%:z") || fmt.contains("%#z")),
        "parse_custom_strftime_assumed_utc format {fmt:?} has an offset spec — an input that \
         carries a real offset must be parsed with parse_custom_strftime, not assumed UTC",
    );
    let naive = chrono::NaiveDateTime::parse_from_str(s, fmt).map_err(|source| {
        TimestampParseError::Invalid {
            input: s.to_string(),
            source,
        }
    })?;
    Ok(IsoOffsetTimestamp(
        Utc.from_utc_datetime(&naive).fixed_offset(),
    ))
}

/// Parse a bare `YYYY-MM-DD` date as **midnight UTC** of that day.
///
/// The one helper whose purpose is to fabricate the missing time-of-day and
/// offset, for human-typed CLI input (`--since 2026-01-15`) where rejecting
/// friendly input is the only alternative. Never for upstream values.
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

/// Coerce an upstream ISO-8601 timestamp into a grid-ready `when_ts`:
/// RFC 3339 with an explicit offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenTsPrecision {
    /// `2026-06-05T19:18:39+00:00` — chat-common, signal.
    Seconds,
    /// `2026-06-05T19:18:39.123+00:00` — beeper.
    Millis,
}

/// An upstream epoch-millis stamp as a grid-ready `when_ts`, or `None` when
/// there is no answer.
///
/// **Both ways of having no answer land on `None`, and that is the point.**
/// A stamp upstream never set and one that is not a representable instant are
/// equally "we do not know when this happened", which is a null column rather
/// than a stand-in.
///
/// `precision` is per-provider and not a free choice: the value reaches
/// `source_fingerprint`, so changing it re-cuts every fingerprint that
/// provider has and re-renders its whole tree.
pub fn when_ts_from_unix_millis(ms: Option<i64>, precision: WhenTsPrecision) -> Option<String> {
    let ms = ms?;
    match IsoOffsetTimestamp::from_unix_millis(ms) {
        Some(t) => Some(match precision {
            WhenTsPrecision::Seconds => t.to_rfc3339_secs(),
            WhenTsPrecision::Millis => t.to_rfc3339_millis(),
        }),
        None => {
            tracing::warn!(
                ms,
                "when_ts_from_unix_millis: epoch-ms is not a representable instant; \
                 when_ts left null"
            );
            None
        }
    }
}

/// Human-readable timestamp for a rendered markdown body.
///
/// Three cases, deliberately spelled differently: a real instant renders
/// normally; an item upstream never stamped says so in words; and a stamp
/// that exists but is not representable keeps its raw value on screen, because
/// "we have a number and it is nonsense" is a different fact from "we have
/// nothing", and the number is the only lead a reader has.
///
/// Display only — nothing derived from this reaches the index, so unlike
/// [`when_ts_from_unix_millis`] the formatting here is safe to change.
pub fn display_ts_from_unix_millis(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "(no timestamp)".to_string();
    };
    IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.inner().format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("@{ms}ms"))
}

/// ISO 8601 but not RFC 3339, so it slips past producers and gets
/// rejected at `GridRow::build`, silently dropping the row's
/// `.grid_rows.json`. This normalizes it; already-valid values pass
/// through **verbatim** so callers that persist them don't churn
/// historical strings.
pub fn coerce_when_ts(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Extended forms (with separators), including bare `Z`, are already
    // grid-valid — keep them exactly as written.
    if validate_iso_offset(s).is_ok() {
        return Some(s.to_string());
    }
    // Basic forms: chrono's `%z` wants a numeric offset, so fold a trailing
    // `Z` (UTC) to `+0000` first, then try with and without sub-seconds.
    let folded = match s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        Some(prefix) => format!("{prefix}+0000"),
        None => s.to_string(),
    };
    for fmt in ["%Y%m%dT%H%M%S%z", "%Y%m%dT%H%M%S%.f%z"] {
        if let Ok(dt) = DateTime::parse_from_str(&folded, fmt) {
            return Some(IsoOffsetTimestamp(dt).to_rfc3339_secs());
        }
    }
    None
}

pub fn split_when_ts(s: &str) -> Option<(String, String)> {
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
    let dt = parse_strict(normalized).ok()?.inner();
    let offset = dt.offset().to_string();
    Some((utc_micros(dt), offset))
}

/// Render an offsetted instant in the canonical `when_ts_utc` form:
/// UTC, fixed microsecond precision, `Z` suffix. The `Z` (rather than
/// `+00:00`) states the intent — *this column is UTC* — instead of a
/// local zone that merely happens to sit at zero offset. The single
/// spelling is also what keeps the column lexically sortable; the
/// original local offset is preserved separately in `when_offset` (see
/// [`split_when_ts`]).
fn utc_micros(dt: DateTime<FixedOffset>) -> String {
    dt.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Normalize a user-typed time bound (the value behind a `before:` / `after:`
/// search filter) into the **same canonical UTC form** as the `when_ts_utc`
/// index column, so the two compare correctly as plain strings.
///
/// **A user-typed timestamp with no offset means local machine time**, since
/// people type wall-clock times in the zone they are sitting in. An explicit
/// offset is honored as given. `None` when the input matches no accepted
/// shape, so the caller drops the bound rather than comparing against
/// garbage. During a spring-forward gap the wall-clock instant does not exist
/// locally; we take the earlier candidate.
pub fn normalize_user_time_to_utc(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 1. Already offset-bearing (RFC 3339, incl. bare `Z`) — honor it.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(utc_micros(dt));
    }
    // 2. Naive date-time → local wall-clock.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
    {
        let local = Local.from_local_datetime(&naive).earliest()?;
        return Some(utc_micros(local.fixed_offset()));
    }
    // 3. Bare date → local midnight.
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = date
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always a valid time-of-day");
        let local = Local.from_local_datetime(&naive).earliest()?;
        return Some(utc_micros(local.fixed_offset()));
    }
    None
}

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
    fn coerce_when_ts_canonicalizes_basic_iso_and_preserves_valid() {
        // Basic ISO 8601 (no separators), Fastmail's vCard REV shape.
        assert_eq!(
            coerce_when_ts("20260605T191839Z").as_deref(),
            Some("2026-06-05T19:18:39+00:00")
        );
        // Basic ISO 8601 with a numeric offset.
        assert_eq!(
            coerce_when_ts("20260605T121839-0700").as_deref(),
            Some("2026-06-05T12:18:39-07:00")
        );
        // Already grid-valid extended forms pass through verbatim (no churn).
        assert_eq!(
            coerce_when_ts("2370-04-15T00:00:00Z").as_deref(),
            Some("2370-04-15T00:00:00Z")
        );
        assert_eq!(
            coerce_when_ts("2026-06-05T12:18:39-07:00").as_deref(),
            Some("2026-06-05T12:18:39-07:00")
        );
        // Unparseable / offset-less → None (caller drops it).
        assert_eq!(coerce_when_ts("not a timestamp"), None);
        assert_eq!(coerce_when_ts("20260605T191839"), None);
        assert_eq!(coerce_when_ts(""), None);
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
    fn parse_custom_strftime_assumed_utc_google_chat_and_linkedin_shapes() {
        // Google Chat's export states its zone as the literal word "UTC".
        let t = parse_custom_strftime_assumed_utc(
            "Tuesday, February 11, 2025 at 11:33:35 AM UTC",
            "%A, %B %d, %Y at %I:%M:%S %p UTC",
        )
        .unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2025-02-11T11:33:35+00:00");
        // LinkedIn's, with the trailing " UTC" already trimmed off.
        let t =
            parse_custom_strftime_assumed_utc("2026-06-16 22:11:33", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2026-06-16T22:11:33+00:00");
        // Unparseable input is an error, never a fabricated stamp.
        assert!(parse_custom_strftime_assumed_utc("", "%Y-%m-%d %H:%M:%S").is_err());
        assert!(
            parse_custom_strftime_assumed_utc("not a date", "%Y-%m-%d %H:%M:%S").is_err(),
            "a shape we don't recognize must not resolve to the epoch",
        );
    }

    #[test]
    fn to_unix_millis_round_trips() {
        let t = parse_strict("2026-06-10T14:23:00-07:00").unwrap();
        assert_eq!(
            IsoOffsetTimestamp::from_unix_millis(t.to_unix_millis())
                .unwrap()
                .to_unix_millis(),
            t.to_unix_millis(),
        );
        assert_eq!(
            parse_strict("1970-01-01T00:00:00Z")
                .unwrap()
                .to_unix_millis(),
            0
        );
    }

    #[test]
    fn parse_yyyy_mm_dd_assumed_utc_explicit() {
        let t = parse_yyyy_mm_dd_assumed_utc("2026-01-15").unwrap();
        assert_eq!(t.to_rfc3339_secs(), "2026-01-15T00:00:00+00:00");
        assert!(parse_yyyy_mm_dd_assumed_utc("not-a-date").is_err());
    }

    #[test]
    fn split_when_ts_normalizes_utc_and_keeps_offset() {
        // Local-offset input: UTC column shifts by the offset and is
        // spelled with `Z`; the offset column preserves the original zone.
        let (utc, off) = split_when_ts("2026-06-10T14:23:00-07:00").unwrap();
        assert_eq!(utc, "2026-06-10T21:23:00.000000Z");
        assert_eq!(off, "-07:00");

        // Already-UTC input (explicit +00:00): UTC column uses `Z`, but
        // the offset column keeps the input's literal `+00:00`.
        let (utc, off) = split_when_ts("2026-06-10T21:23:00+00:00").unwrap();
        assert_eq!(utc, "2026-06-10T21:23:00.000000Z");
        assert_eq!(off, "+00:00");

        // Bare `Z` input is tolerated; offset column reports `+00:00`.
        let (utc, off) = split_when_ts("2026-06-10T21:23:00Z").unwrap();
        assert_eq!(utc, "2026-06-10T21:23:00.000000Z");
        assert_eq!(off, "+00:00");

        // Half-hour offset round-trips.
        let (_utc, off) = split_when_ts("2026-06-10T21:23:00+05:30").unwrap();
        assert_eq!(off, "+05:30");

        // Far-future (24th-century TNG-era) dates parse and render fine —
        // chrono supports 4-digit years through 9999, so the fixture
        // corpus's stardate-era timestamps are well within range.
        let (utc, off) = split_when_ts("2369-04-15T14:00:00-07:00").unwrap();
        assert_eq!(utc, "2369-04-15T21:00:00.000000Z");
        assert_eq!(off, "-07:00");

        // Empty / unparseable → None, so the caller leaves columns NULL.
        assert!(split_when_ts("").is_none());
        assert!(split_when_ts("2026-06-10T21:23:00").is_none());
    }

    #[test]
    fn split_when_ts_utc_column_sorts_chronologically() {
        // Two instants whose raw `when_ts` strings sort the *opposite* way
        // from true chronological order, because the offsets differ:
        //   a = 2026-01-01T23:00:00+00:00  → 23:00 UTC (the later instant)
        //   b = 2026-01-02T00:00:00+05:00  → 19:00 UTC (the earlier instant)
        // As plain text a < b (date "...01T23" < "...02T00"), yet b happens
        // four hours before a. The UTC column must put b first.
        let a = "2026-01-01T23:00:00+00:00";
        let b = "2026-01-02T00:00:00+05:00";
        let (a_utc, _) = split_when_ts(a).unwrap();
        let (b_utc, _) = split_when_ts(b).unwrap();
        assert!(
            a < b,
            "raw strings mis-sort: text order puts the later instant first"
        );
        assert!(b_utc < a_utc, "UTC column sorts by true instant");
    }

    #[test]
    fn normalize_user_time_honors_explicit_offset() {
        // An explicit offset is honored and converted to UTC (spelled `Z`).
        assert_eq!(
            normalize_user_time_to_utc("2026-01-15T00:00:00-08:00").as_deref(),
            Some("2026-01-15T08:00:00.000000Z")
        );
        // Bare `Z` is accepted and stays `Z`.
        assert_eq!(
            normalize_user_time_to_utc("2026-01-15T12:00:00Z").as_deref(),
            Some("2026-01-15T12:00:00.000000Z")
        );
        assert!(normalize_user_time_to_utc("not-a-date").is_none());
        assert!(normalize_user_time_to_utc("   ").is_none());
    }

    #[test]
    fn normalize_user_time_assumes_local_for_naive() {
        // Naive input is interpreted in the local machine zone. We verify
        // by computing the same instant through chrono's `Local`
        // independently, so the test is machine-timezone-agnostic.
        let expect_local = |y, mo, d, h, mi| {
            let naive = NaiveDate::from_ymd_opt(y, mo, d)
                .unwrap()
                .and_hms_opt(h, mi, 0)
                .unwrap();
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Micros, true)
        };
        assert_eq!(
            normalize_user_time_to_utc("2026-01-15T09:30:00").as_deref(),
            Some(expect_local(2026, 1, 15, 9, 30).as_str())
        );
        // Bare date → local midnight.
        assert_eq!(
            normalize_user_time_to_utc("2026-01-15").as_deref(),
            Some(expect_local(2026, 1, 15, 0, 0).as_str())
        );
    }

    #[test]
    fn validate_iso_offset_rejects_naive() {
        assert!(validate_iso_offset("2026-06-10T14:23:00-07:00").is_ok());
        assert!(validate_iso_offset("2026-06-10T21:23:00").is_err());
    }
}
