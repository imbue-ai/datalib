//! Two date-time parsers used by the Takeout walkers.

use chrono::{NaiveDateTime, TimeZone};
use datalib_time::IsoOffsetTimestamp;

/// The offset east of UTC, in minutes, of the zone Google names at the
/// end of a Takeout timestamp. Google writes the short name the account's
/// English locale has for the zone (`PDT`, `CEST`, `AEST`), and for a zone
/// with none, the offset itself (`GMT+2`, `GMT+5:30`). A name two zones
/// share (`IST`: India, Ireland, Israel) is left out, so its entries are
/// reported rather than misdated.
fn tz_abbrev_offset_minutes(abbr: &str) -> Option<i32> {
    if let Some(offset) = gmt_offset_minutes(abbr) {
        return Some(offset);
    }
    let hours = |h: f32| Some((h * 60.0) as i32);
    match abbr {
        "WET" => hours(0.0),
        "BST" | "WEST" | "CET" => hours(1.0),
        "CEST" | "EET" => hours(2.0),
        "EEST" | "MSK" => hours(3.0),
        "AWST" => hours(8.0),
        "JST" | "KST" => hours(9.0),
        "ACST" => hours(9.5),
        "AEST" => hours(10.0),
        "ACDT" => hours(10.5),
        "AEDT" => hours(11.0),
        "NZST" => hours(12.0),
        "NZDT" => hours(13.0),
        "NST" => hours(-3.5),
        "NDT" => hours(-2.5),
        "AST" => hours(-4.0),
        "ADT" => hours(-3.0),
        "HDT" => hours(-9.0),
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

/// `GMT`, `UTC`, `GMT+2`, `GMT-3`, `GMT+5:30`.
fn gmt_offset_minutes(abbr: &str) -> Option<i32> {
    let rest = abbr
        .strip_prefix("GMT")
        .or_else(|| abbr.strip_prefix("UTC"))?;
    if rest.is_empty() {
        return Some(0);
    }
    let (sign, rest) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if !(0..=14).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some(sign * (h * 60 + m))
}

/// Normalize the Unicode spaces Google sprinkles into recent exports
/// to plain ASCII spaces. Newer Takeout `created_date`/grid timestamps
/// use a narrow no-break space (U+202F) — and occasionally a regular
/// no-break space (U+00A0) — before the AM/PM marker, which the chrono
/// format's literal space separator does not reliably match. No-op for
/// the older all-ASCII strings.
fn normalize_spaces(s: &str) -> String {
    s.replace(['\u{202f}', '\u{00a0}'], " ")
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

pub fn parse_chat_long_form(s: &str) -> Option<String> {
    let s = normalize_spaces(s);
    let (body, abbr) = split_trailing_abbrev(&s)?;
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

pub fn parse_mdl_grid(s: &str) -> Option<String> {
    let s = normalize_spaces(s);
    let (body, abbr) = split_trailing_abbrev(&s)?;
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
        datalib_time::parse_strict(&out).expect("rfc3339 with offset");
        assert!(out.starts_with("2025-02-11T11:33:35"));
        assert!(out.ends_with("+00:00") || out.ends_with("Z"));
    }

    #[test]
    fn chat_long_form_narrow_no_break_space() {
        // Recent exports use U+202F before AM/PM; must still parse.
        let out = parse_chat_long_form("Tuesday, February 11, 2025 at 11:33:35\u{202f}AM UTC")
            .expect("should parse with narrow no-break space");
        assert!(out.starts_with("2025-02-11T11:33:35"));
    }

    #[test]
    fn mdl_grid_pdt_pm() {
        let out = parse_mdl_grid("Jun 4, 2026, 11:48:37 PM PDT").expect("should parse");
        datalib_time::parse_strict(&out).expect("rfc3339 with offset");
        assert!(out.starts_with("2026-06-04T23:48:37"));
        assert!(out.ends_with("-07:00"));
    }

    #[test]
    fn mdl_grid_est_am() {
        let out = parse_mdl_grid("Jan 3, 2026, 9:15:00 AM EST").expect("parse");
        assert!(out.starts_with("2026-01-03T09:15:00"));
        assert!(out.ends_with("-05:00"));
    }

    /// An export from an English locale outside North America names its
    /// zone `CEST`; every Gemini entry in one went unread, and every
    /// YouTube watch landed with no time.
    #[test]
    fn mdl_grid_european_summer_time() {
        let out = parse_mdl_grid("Jun 4, 2026, 8:29:39\u{202f}PM CEST").expect("parse");
        assert_eq!(out, "2026-06-04T20:29:39+02:00");
        let out =
            parse_chat_long_form("Tuesday, February 11, 2025 at 11:33:35 AM CET").expect("parse");
        assert_eq!(out, "2025-02-11T11:33:35+01:00");
    }

    #[test]
    fn a_zone_with_no_name_is_its_offset() {
        let at = |zone: &str| parse_mdl_grid(&format!("Jun 4, 2026, 8:29:39 PM {zone}"));
        assert_eq!(at("GMT+2").as_deref(), Some("2026-06-04T20:29:39+02:00"));
        assert_eq!(at("GMT+5:30").as_deref(), Some("2026-06-04T20:29:39+05:30"));
        assert_eq!(at("GMT-3").as_deref(), Some("2026-06-04T20:29:39-03:00"));
        assert_eq!(at("NST").as_deref(), Some("2026-06-04T20:29:39-03:30"));
        assert_eq!(at("GMT").as_deref(), Some("2026-06-04T20:29:39+00:00"));
        assert_eq!(at("GMT+25"), None);
        assert_eq!(at("GMT+2:75"), None);
        assert_eq!(at("GMTX"), None);
    }

    #[test]
    fn a_name_two_zones_share_yields_none() {
        assert!(parse_mdl_grid("Jun 4, 2026, 11:48:37 AM IST").is_none());
        assert!(parse_mdl_grid("Jun 4, 2026, 11:48:37 AM XYZT").is_none());
    }

    #[test]
    fn malformed_input_yields_none() {
        assert!(parse_mdl_grid("not a timestamp").is_none());
        assert!(parse_chat_long_form("").is_none());
    }
}
