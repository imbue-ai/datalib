//! RFC 3339 read and written over jiff's types, accepting and spelling
//! exactly what chrono's `parse_from_rfc3339` and `to_rfc3339_opts` do,
//! so that every stamp already stored keeps its bytes. One difference is
//! the point of this module: `Z` and `-00:00` read as a clock nobody
//! named ([`Clock::Unknown`]), `+00:00` as UTC's.

use jiff::{civil, tz::Offset, SignedDuration, Timestamp};

/// The clock a stamp was written on (RFC 9557 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    Known(Offset),
    /// `Z` (or RFC 3339's `-00:00`): the instant is known, whose wall
    /// clock it was is not.
    Unknown,
}

impl Clock {
    /// The offset to show the instant in: UTC's when no clock is known.
    pub(crate) fn offset(self) -> Offset {
        match self {
            Clock::Known(o) => o,
            Clock::Unknown => Offset::UTC,
        }
    }
}

/// An offset RFC 3339 can write: within a day either side of UTC, as
/// chrono's `FixedOffset` holds them.
pub(crate) fn offset_from_secs(secs: i32) -> Option<Offset> {
    if secs.abs() >= 86_400 {
        return None;
    }
    Offset::from_seconds(secs).ok()
}

// chrono's words for each failure, so a problem row reads as it did.
pub(crate) const OUT_OF_RANGE: &str = "input is out of range";
const INVALID: &str = "input contains invalid characters";
const TOO_SHORT: &str = "premature end of input";
const TOO_LONG: &str = "trailing input";

/// `YYYY-MM-DD(T|t| )HH:MM:SS[.f+](Z|z|±HH:MM)`. Fraction digits past
/// the ninth are skipped; a leap second (`:60`) is the next minute's
/// first second, the instant chrono's epoch millis gave it.
pub(crate) fn parse(s: &str) -> Result<(Timestamp, Clock), &'static str> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return Err(TOO_SHORT);
    }
    let digit = |i: usize| -> Result<i32, &'static str> {
        match b[i] {
            d @ b'0'..=b'9' => Ok(i32::from(d - b'0')),
            _ => Err(INVALID),
        }
    };
    let two = |i: usize| -> Result<i32, &'static str> { Ok(digit(i)? * 10 + digit(i + 1)?) };
    let expect = |i: usize, c: u8| if b[i] == c { Ok(()) } else { Err(INVALID) };

    let year = two(0)? * 100 + two(2)?;
    expect(4, b'-')?;
    let month = two(5)?;
    expect(7, b'-')?;
    let day = two(8)?;
    let date = civil::Date::new(year as i16, month as i8, day as i8).map_err(|_| OUT_OF_RANGE)?;
    if !matches!(b[10], b'T' | b't' | b' ') {
        return Err(INVALID);
    }
    let hour = two(11)?;
    expect(13, b':')?;
    let minute = two(14)?;
    expect(16, b':')?;
    let (second, leap) = match two(17)? {
        60 => (59, true),
        s => (s, false),
    };

    let mut rest = &s[19..];
    let mut nanos = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(if frac.is_empty() { TOO_SHORT } else { INVALID });
        }
        let kept = &frac[..digits.min(9)];
        nanos = kept.parse::<i32>().expect("nine digits fit") * 10_i32.pow(9 - kept.len() as u32);
        rest = &frac[digits..];
    }

    if hour > 23 || minute > 59 || second > 59 {
        return Err(OUT_OF_RANGE);
    }
    let (negative, offset_secs, rest) = offset(rest)?;
    if !rest.is_empty() {
        return Err(TOO_LONG);
    }
    let clock = match (negative, offset_secs) {
        (None, _) | (Some(true), 0) => Clock::Unknown,
        (Some(true), s) => Clock::Known(offset_from_secs(-s).ok_or(OUT_OF_RANGE)?),
        (Some(false), s) => Clock::Known(offset_from_secs(s).ok_or(OUT_OF_RANGE)?),
    };
    let wall = date.at(hour as i8, minute as i8, second as i8, nanos);
    let at = clock
        .offset()
        .to_timestamp(wall)
        .map_err(|_| OUT_OF_RANGE)?;
    let at = if leap {
        at.checked_add(SignedDuration::from_secs(1))
            .map_err(|_| OUT_OF_RANGE)?
    } else {
        at
    };
    Ok((at, clock))
}

/// `Z` as no sign, else whether the offset is west of UTC, and its
/// seconds; in the order chrono checks, so a refusal names the same fault.
fn offset(s: &str) -> Result<(Option<bool>, i32, &str), &'static str> {
    if let Some(rest) = s.strip_prefix(['Z', 'z']) {
        return Ok((None, 0, rest));
    }
    let (negative, s) = if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else if let Some(r) = s.strip_prefix('-').or_else(|| s.strip_prefix('\u{2212}')) {
        (true, r)
    } else if s.is_empty() {
        return Err(TOO_SHORT);
    } else {
        return Err(INVALID);
    };
    let b = s.as_bytes();
    let n = |d: u8| i32::from(d - b'0');
    let hours = match b {
        [h1 @ b'0'..=b'9', h2 @ b'0'..=b'9', ..] => n(*h1) * 10 + n(*h2),
        [_, _, ..] => return Err(INVALID),
        _ => return Err(TOO_SHORT),
    };
    match b.get(2) {
        Some(b':') => {}
        Some(_) => return Err(INVALID),
        None => return Err(TOO_SHORT),
    }
    let minutes = match &b[3..] {
        [m1 @ b'0'..=b'5', m2 @ b'0'..=b'9', ..] => n(*m1) * 10 + n(*m2),
        [b'6'..=b'9', b'0'..=b'9', ..] => return Err(OUT_OF_RANGE),
        [_, _, ..] => return Err(INVALID),
        _ => return Err(TOO_SHORT),
    };
    Ok((Some(negative), hours * 3600 + minutes * 60, &s[5..]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Precision {
    /// None, three, six or nine digits: as few as say the instant exactly.
    Auto,
    Secs,
    Millis,
    Micros,
}

/// How a known UTC clock is spelled: `+00:00`, or `Z` for a column that
/// is UTC by definition (`created_at_utc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Utc {
    Numeric,
    Zulu,
}

pub(crate) fn write(at: Timestamp, offset: Offset, precision: Precision, utc: Utc) -> String {
    let wall = offset.to_datetime(at);
    let year = i32::from(wall.year());
    let mut out = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+05}")
    };
    out += &format!(
        "-{:02}-{:02}T{:02}:{:02}:{:02}",
        wall.month(),
        wall.day(),
        wall.hour(),
        wall.minute(),
        wall.second()
    );
    let nano = wall.subsec_nanosecond();
    match precision {
        Precision::Secs => {}
        Precision::Millis => out += &format!(".{:03}", nano / 1_000_000),
        Precision::Micros => out += &format!(".{:06}", nano / 1_000),
        Precision::Auto if nano == 0 => {}
        Precision::Auto if nano % 1_000_000 == 0 => out += &format!(".{:03}", nano / 1_000_000),
        Precision::Auto if nano % 1_000 == 0 => out += &format!(".{:06}", nano / 1_000),
        Precision::Auto => out += &format!(".{nano:09}"),
    }
    let secs = offset.seconds();
    if utc == Utc::Zulu && secs == 0 {
        out.push('Z');
        return out;
    }
    let sign = if secs < 0 { '-' } else { '+' };
    let minutes = (secs.abs() + 30) / 60;
    out += &format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60);
    out
}

/// `+02:00`, or `+05:53:28` for an offset with seconds: a `tz_offset`
/// column's spelling.
pub(crate) fn offset_text(offset: Offset) -> String {
    let secs = offset.seconds();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.abs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, FixedOffset, SecondsFormat};

    /// Every spelling chrono reads, read here to the same instant, and
    /// every spelling it refuses, refused with the same words.
    #[test]
    fn reads_what_chrono_reads() {
        for s in [
            "2026-06-10T14:23:00-07:00",
            "2026-06-10t14:23:00+05:30",
            "2026-06-10 14:23:00Z",
            "2026-06-10T14:23:00z",
            "2026-06-10T14:23:00.5+01:00",
            "2026-06-10T14:23:00.123456789-00:00",
            "2026-06-10T14:23:00.1234567891234Z",
            "2026-06-10T14:23:00\u{2212}03:00",
            "2016-12-31T23:59:60Z",
            "2016-12-31T23:59:60.5+00:00",
            "0000-01-01T00:00:00+23:59",
            "9999-12-30T21:59:59.999999999Z",
            "1969-12-31T23:59:59.999-00:00",
        ] {
            let ours = parse(s).unwrap_or_else(|e| panic!("{s}: {e}")).0;
            let theirs = DateTime::parse_from_rfc3339(s).unwrap();
            assert_eq!(crate::unix_millis(ours), theirs.timestamp_millis(), "{s}");
        }
        for s in [
            "",
            "2026-06-10",
            "2026-06-10T14:23:00",
            "2026-06-10T14:23:00.Z",
            "2026-06-10T14:23:00.",
            "2026-06-10T14:23:00+0700",
            "2026-06-10T14:23:00+07",
            "2026-06-10T14:23:00+07:60",
            "2026-06-10T14:23:00+24:00",
            "2026-06-10T24:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-06-10T14:23:00Z ",
            "2026-06-10X14:23:00Z",
            "20260610T142300Z",
            "+2026-06-10T14:23:00Z",
        ] {
            let theirs = DateTime::parse_from_rfc3339(s).unwrap_err().to_string();
            assert_eq!(parse(s).err(), Some(theirs.as_str()), "{s:?}");
        }
    }

    /// Past jiff's last instant chrono still reads a stamp; this refuses it.
    #[test]
    fn the_last_day_of_9999_is_past_the_range() {
        assert!(DateTime::parse_from_rfc3339("9999-12-31T23:59:59Z").is_ok());
        assert_eq!(parse("9999-12-31T23:59:59Z").err(), Some(OUT_OF_RANGE));
    }

    #[test]
    fn z_and_minus_zero_name_no_clock() {
        assert_eq!(parse("2026-06-10T14:23:00Z").unwrap().1, Clock::Unknown);
        assert_eq!(
            parse("2026-06-10T14:23:00-00:00").unwrap().1,
            Clock::Unknown
        );
        assert_eq!(
            parse("2026-06-10T14:23:00+00:00").unwrap().1,
            Clock::Known(Offset::UTC)
        );
    }

    /// Every precision writes what chrono writes for the same instant.
    #[test]
    fn writes_what_chrono_writes() {
        let cases = [
            ("2026-06-10T14:23:00-07:00", -7 * 3600),
            ("2026-06-10T14:23:00.120+05:30", 19_800),
            ("2026-06-10T14:23:00.000001+00:00", 0),
            ("2026-06-10T14:23:00.00000012+00:00", 0),
            ("2026-06-10T14:23:00.123456789+01:00", 3600),
            ("1969-12-31T23:59:59.999-03:00", -3 * 3600),
            ("0000-01-01T00:00:00+00:00", 0),
            ("2026-06-10T14:23:00+05:53", 21_208),
        ];
        let forms = [
            (Precision::Auto, SecondsFormat::AutoSi),
            (Precision::Secs, SecondsFormat::Secs),
            (Precision::Millis, SecondsFormat::Millis),
            (Precision::Micros, SecondsFormat::Micros),
        ];
        for (s, offset_secs) in cases {
            let (at, _) = parse(s).unwrap();
            let offset = Offset::from_seconds(offset_secs).unwrap();
            let theirs = DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&FixedOffset::east_opt(offset_secs).unwrap());
            for (ours_p, theirs_p) in forms {
                for (utc, z) in [(Utc::Numeric, false), (Utc::Zulu, true)] {
                    assert_eq!(
                        write(at, offset, ours_p, utc),
                        theirs.to_rfc3339_opts(theirs_p, z),
                        "{s} {ours_p:?} {utc:?}"
                    );
                }
            }
            assert_eq!(offset_text(offset), theirs.offset().to_string(), "{s}");
        }
    }

    #[test]
    fn an_offset_rfc_3339_cannot_write_is_none() {
        assert!(offset_from_secs(86_399).is_some());
        assert!(offset_from_secs(-86_399).is_some());
        assert!(offset_from_secs(86_400).is_none());
    }
}
