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
//! See [`docs/dev/data_architecture_plan.md`](/docs/dev/data_architecture_plan.md)
//! §P0.5 for the architectural backstory, and
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
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

/// Split a stored `when_ts` (RFC 3339 with an explicit offset;
/// tolerates `Z`) into the two values the `grid_rows` index needs:
///
/// * `.0` — the same instant normalized to **UTC**, rendered with fixed
///   microsecond precision and a `Z` suffix (the `Z` states "this is
///   UTC", not a local zone that happens to sit at zero offset). Because
///   every value shares one zone and one width, lexical ordering of this
///   column matches true chronological order — which a column of mixed
///   local-offset `when_ts` strings does *not*: `2026-01-01T09:00:00+00:00`
///   sorts before `2026-01-01T10:00:00-08:00` as text, yet is nine hours
///   *earlier* in absolute time.
/// * `.1` — the original UTC offset (`+05:30`, `-07:00`, `+00:00`),
///   preserved so the UI can re-render the instant in the wall-clock
///   zone it was recorded in.
///
/// Returns `None` on empty input or parse failure, so the caller can
/// leave both index columns NULL rather than fabricate a value.
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

/// Normalize a user-typed time bound (the value behind a `before:` /
/// `after:` search filter) into the **same canonical UTC form** as the
/// `when_ts_utc` index column, so the two compare correctly as plain
/// strings.
///
/// Policy: **a user-typed timestamp with no offset means local machine
/// time.** People type wall-clock times in the zone they're sitting in,
/// not UTC, so `before:2026-01-15` means "before midnight here" and
/// `after:2026-01-15T09:00` means "after 9am here" — and we convert that
/// to UTC before comparing against the (UTC) index. An input that
/// *does* carry an explicit offset is honored as written. This is the
/// query-side mirror of [`parse_with_assumed_utc`] (which assumes UTC
/// for audited *upstream* feeds): here the human is the source, so local
/// time is the right assumption, not UTC.
///
/// Accepts:
/// - bare date `YYYY-MM-DD` → local midnight that day
/// - naive date-time `YYYY-MM-DDTHH:MM:SS[.fff]` → that local wall-clock
/// - explicit-offset RFC 3339 (`...-07:00`, `...Z`) → honored as-is
///
/// Returns `None` when the input matches none of those shapes, so the
/// caller can drop the bound rather than compare against a garbage
/// string. During a spring-forward gap the wall-clock instant doesn't
/// exist locally; we take the earlier of the two candidate instants.
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
