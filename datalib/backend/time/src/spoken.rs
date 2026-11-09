//! A day, or a day and a time, as a person writes or pastes one into a
//! search: `Tuesday, April 3, 2026`, `3 Apr 2026 at 9:30 pm`, and the
//! form datalib itself shows, `Tue Apr 15th, 2369 at 08:30`. English month
//! and weekday names only. A year is required: `April 3` alone could be
//! any of them.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const TIMES: &[&str] = &["%H:%M", "%I:%M %p", "%I:%M%p"];

/// The wall-clock time `s` names, midnight for a bare day. `None` for
/// anything else, and for a weekday the date does not fall on.
pub(crate) fn spoken(s: &str) -> Option<NaiveDateTime> {
    let lower = s.to_lowercase().replace(',', " ");
    let mut words: Vec<&str> = lower
        .split_whitespace()
        .filter(|w| *w != "at")
        .map(without_ordinal)
        .collect();
    let weekday = match words.first().map(|w| w.parse::<Weekday>()) {
        Some(Ok(day)) => {
            words.remove(0);
            Some(day)
        }
        _ => None,
    };
    let [a, b, year, time @ ..] = words.as_slice() else {
        return None;
    };
    let (month, day) = match (month(a), month(b)) {
        (Some(m), None) => (m, b),
        (None, Some(m)) => (m, a),
        _ => return None,
    };
    if year.len() != 4 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year.parse().ok()?, month, day.parse().ok()?)?;
    let time = if time.is_empty() {
        NaiveTime::MIN
    } else {
        let mut time = time.join(" ").to_uppercase();
        // chrono reads no hour without its minutes: `9PM` is `9:00PM`.
        if !time.contains(':') {
            let hour = time.bytes().take_while(u8::is_ascii_digit).count();
            time.insert_str(hour, ":00");
        }
        TIMES
            .iter()
            .find_map(|f| NaiveTime::parse_from_str(&time, f).ok())?
    };
    weekday
        .is_none_or(|d| date.weekday() == d)
        .then(|| date.and_time(time))
}

/// `april`, `apr` or `sept` → 4 or 9.
fn month(word: &str) -> Option<u32> {
    MONTHS
        .iter()
        .position(|m| word.len() >= 3 && (m.starts_with(word)))
        .map(|i| i as u32 + 1)
}

/// `15th` → `15`; any other word as it is.
fn without_ordinal(word: &str) -> &str {
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &word[digits.len()..];
    let is_ordinal = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && ["st", "nd", "rd", "th"].contains(&suffix);
    if is_ordinal {
        digits
    } else {
        word
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> Option<NaiveDateTime> {
        NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(h, min, 0)
    }

    #[test]
    fn reads_a_day_as_people_write_it() {
        for s in [
            "Tuesday, April 3, 2029",
            "tuesday april 3 2029",
            "April 3, 2029",
            "Apr 3rd 2029",
            "3 April 2029",
            "Tue, 3 Apr 2029",
        ] {
            assert_eq!(spoken(s), at(2029, 4, 3, 0, 0), "{s}");
        }
    }

    #[test]
    fn reads_a_time_after_the_day() {
        assert_eq!(
            spoken("Tue Apr 15th, 2369 at 08:30"),
            at(2369, 4, 15, 8, 30),
            "what short_ts writes"
        );
        assert_eq!(spoken("April 3, 2029 9:30 PM"), at(2029, 4, 3, 21, 30));
        assert_eq!(spoken("3 Apr 2029 at 9pm"), at(2029, 4, 3, 21, 0));
    }

    #[test]
    fn refuses_a_weekday_the_date_is_not_and_a_day_with_no_year() {
        assert_eq!(spoken("Monday, April 3, 2029"), None);
        assert_eq!(spoken("April 3"), None);
        assert_eq!(spoken("yesterday"), None);
        assert_eq!(spoken("April 31, 2029"), None);
    }
}
