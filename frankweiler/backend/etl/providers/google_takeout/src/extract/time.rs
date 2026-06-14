//! Two date-time parsers used by the Takeout walkers.
//!
//! Google emits two human-grade timestamp shapes in the slices we
//! ingest:
//!
//!   1. **Google Chat long-form English** (in `messages.json`'s
//!      `created_date`):
//!      `"Tuesday, February 11, 2025 at 11:33:35 AM UTC"`
//!   2. **Takeout MDL grid timestamp** (YouTube watch history + Gemini
//!      activity cells):
//!      `"Jun 4, 2026, 11:48:37 AM PDT"`
//!
//! Both carry the timezone as a North-American three/four-letter
//! abbreviation. We hard-code the abbreviations Google emits to
//! fixed offsets and refuse to guess on anything else. A parse
//! failure surfaces as `None` so the caller can leave `when_ts =
//! NULL` per the architecture doc's "no fabricated timestamps" rule.

use chrono::{NaiveDateTime, TimeZone};
use frankweiler_time::IsoOffsetTimestamp;

/// Map one of the North-American timezone abbreviations Google emits
/// in Takeout exports to a fixed-offset minute count east of UTC.
/// Returns `None` for any abbreviation we haven't audited.
fn tz_abbrev_offset_minutes(abbr: &str) -> Option<i32> {
    match abbr {
        "UTC" | "GMT" => Some(0),
        "EST" => Some(-5 * 60),
        "EDT" => Some(-4 * 60),
        "CST" => Some(-6 * 60),
        "CDT" => Some(-5 * 60),
        "MST" => Some(-7 * 60),
        "MDT" => Some(-6 * 60),
        "PST" => Some(-8 * 60),
        "PDT" => Some(-7 * 60),
        "AKST" => Some(-9 * 60),
        "AKDT" => Some(-8 * 60),
        "HST" => Some(-10 * 60),
        _ => None,
    }
}

/// Split a timestamp string like `"Jun 4, 2026, 11:48:37 AM PDT"`
/// into `(body, tz_abbreviation)` by peeling off the trailing
/// whitespace-separated token. Returns `None` when there's no
/// whitespace to split on.
fn split_trailing_abbrev(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let idx = s.rfind(char::is_whitespace)?;
    let body = s[..idx].trim_end();
    let abbr = s[idx + 1..].trim();
    if body.is_empty() || abbr.is_empty() {
        return None;
    }
    Some((body, abbr))
}

fn finalize(naive: NaiveDateTime, offset_minutes: i32) -> Option<String> {
    let offset = chrono::FixedOffset::east_opt(offset_minutes * 60)?;
    let dt = offset.from_local_datetime(&naive).single()?;
    Some(IsoOffsetTimestamp::from(dt).to_rfc3339())
}

/// Parse Google Chat's `created_date` field, e.g.
/// `"Tuesday, February 11, 2025 at 11:33:35 AM UTC"`. Returns
/// `Some(rfc3339)` on success.
pub fn parse_chat_long_form(s: &str) -> Option<String> {
    let (body, abbr) = split_trailing_abbrev(s)?;
    let offset = tz_abbrev_offset_minutes(abbr)?;
    // Strip the weekday prefix ("Tuesday, ") — chrono can't parse
    // English weekdays alongside a full date in one shot, and the
    // weekday is redundant with the date anyway.
    let after_weekday = body.split_once(", ").map(|(_, rest)| rest).unwrap_or(body);
    // Drop the " at " separator between date and time.
    let normalized = after_weekday.replacen(" at ", " ", 1);
    let naive = NaiveDateTime::parse_from_str(&normalized, "%B %e, %Y %l:%M:%S %p")
        .or_else(|_| NaiveDateTime::parse_from_str(&normalized, "%B %d, %Y %l:%M:%S %p"))
        .ok()?;
    finalize(naive, offset)
}

/// Parse a Takeout MDL grid timestamp, e.g.
/// `"Jun 4, 2026, 11:48:37 AM PDT"` (YouTube watch-history,
/// Gemini activity cells).
pub fn parse_mdl_grid(s: &str) -> Option<String> {
    let (body, abbr) = split_trailing_abbrev(s)?;
    let offset = tz_abbrev_offset_minutes(abbr)?;
    let normalized = body.replace(',', "");
    // `%b` parses short month names ("Jun"); `%l` is a 12-hour clock
    // with a leading space for single-digit hours.
    let naive = NaiveDateTime::parse_from_str(&normalized, "%b %e %Y %l:%M:%S %p")
        .or_else(|_| NaiveDateTime::parse_from_str(&normalized, "%b %d %Y %l:%M:%S %p"))
        .ok()?;
    finalize(naive, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_long_form_utc() {
        let out = parse_chat_long_form("Tuesday, February 11, 2025 at 11:33:35 AM UTC")
            .expect("should parse");
        // Round-trip via parse_strict to confirm it's a valid RFC
        // 3339 string with an explicit offset.
        frankweiler_time::parse_strict(&out).expect("rfc3339 with offset");
        assert!(out.starts_with("2025-02-11T11:33:35"));
        assert!(out.ends_with("+00:00") || out.ends_with("Z"));
    }

    #[test]
    fn mdl_grid_pdt_pm() {
        let out = parse_mdl_grid("Jun 4, 2026, 11:48:37 PM PDT").expect("should parse");
        frankweiler_time::parse_strict(&out).expect("rfc3339 with offset");
        assert!(out.starts_with("2026-06-04T23:48:37"));
        assert!(out.ends_with("-07:00"));
    }

    #[test]
    fn mdl_grid_est_am() {
        let out = parse_mdl_grid("Jan 3, 2026, 9:15:00 AM EST").expect("parse");
        assert!(out.starts_with("2026-01-03T09:15:00"));
        assert!(out.ends_with("-05:00"));
    }

    #[test]
    fn unknown_tz_abbreviation_yields_none() {
        assert!(parse_mdl_grid("Jun 4, 2026, 11:48:37 AM BST").is_none());
        assert!(parse_chat_long_form("Tuesday, February 11, 2025 at 11:33:35 AM CET").is_none());
    }

    #[test]
    fn malformed_input_yields_none() {
        assert!(parse_mdl_grid("not a timestamp").is_none());
        assert!(parse_chat_long_form("").is_none());
    }
}
