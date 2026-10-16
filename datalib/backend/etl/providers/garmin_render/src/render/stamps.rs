//! Garmin writes most stamps twice, as a GMT and a local wall-clock
//! reading with no offset on either; the offset is their difference.

use chrono::{FixedOffset, NaiveDate, NaiveDateTime, TimeZone};

fn naive(s: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
}

/// Seconds east of UTC, from one moment read in GMT and in local time.
pub fn offset_secs(gmt: &str, local: &str) -> Option<i32> {
    let secs = (naive(local)? - naive(gmt)?).num_seconds();
    let secs = i32::try_from(secs).ok()?;
    FixedOffset::east_opt(secs).map(|_| secs)
}

/// The local wall-clock `local`, `offset` seconds east of UTC, as
/// RFC 3339.
pub fn local_iso(local: &str, offset: i32) -> Option<String> {
    let tz = FixedOffset::east_opt(offset)?;
    tz.from_local_datetime(&naive(local)?)
        .single()
        .map(|t| t.to_rfc3339())
}

/// Midnight starting `date` (`YYYY-MM-DD`) in a zone `offset` seconds
/// east of UTC, as RFC 3339.
pub fn day_start_iso(date: &str, offset: i32) -> Option<String> {
    let midnight = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?;
    let tz = FixedOffset::east_opt(offset)?;
    tz.from_local_datetime(&midnight)
        .single()
        .map(|t| t.to_rfc3339())
}

/// Epoch milliseconds of a GMT reading.
pub fn gmt_ms(gmt: &str) -> Option<i64> {
    Some(naive(gmt)?.and_utc().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_is_local_minus_gmt_in_either_spelling() {
        assert_eq!(
            offset_secs("2369-04-13 06:30:00", "2369-04-13 07:30:00"),
            Some(3600)
        );
        assert_eq!(
            offset_secs("2369-04-12T23:00:00.0", "2369-04-13T00:00:00.0"),
            Some(3600)
        );
        assert_eq!(
            offset_secs("2369-04-13 06:30:00", "2369-04-13 01:00:00"),
            Some(-5 * 3600 - 1800)
        );
        assert_eq!(offset_secs("yesterday", "2369-04-13 01:00:00"), None);
    }

    #[test]
    fn a_local_reading_keeps_its_wall_clock_and_gains_the_offset() {
        assert_eq!(
            local_iso("2369-04-13 07:30:00", 3600).as_deref(),
            Some("2369-04-13T07:30:00+01:00")
        );
        assert_eq!(
            day_start_iso("2369-04-13", -7 * 3600).as_deref(),
            Some("2369-04-13T00:00:00-07:00")
        );
        assert_eq!(
            gmt_ms("1970-01-01 00:00:01"),
            Some(1000),
            "GMT readings are epoch-based"
        );
    }
}
