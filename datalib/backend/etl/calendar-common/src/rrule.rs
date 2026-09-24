//! An RFC 5545 `RRULE` in words: `FREQ=WEEKLY;BYDAY=MO,TH` → "Weekly
//! on Monday and Thursday". What the sentence cannot say is appended
//! verbatim, so nothing in a rule is silently dropped.

use crate::when::EventTime;

/// `rule` is an `RRULE` value; `tz` is the zone the series starts in,
/// so an `UNTIL` in UTC is shown as the date it falls on there.
pub fn describe(rule: &str, tz: Option<&str>) -> String {
    let mut freq: Option<&str> = None;
    let mut interval: u32 = 1;
    let mut count: Option<&str> = None;
    let mut until: Option<&str> = None;
    let mut by_day: Vec<&str> = Vec::new();
    let mut by_month_day: Vec<&str> = Vec::new();
    let mut by_month: Vec<&str> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    for part in rule.trim().trim_start_matches("RRULE:").split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(v),
            "INTERVAL" => match v.parse() {
                Ok(n) => interval = n,
                Err(_) => rest.push(part),
            },
            "COUNT" => count = Some(v),
            "UNTIL" => until = Some(v),
            "BYDAY" => by_day = v.split(',').collect(),
            "BYMONTHDAY" => by_month_day = v.split(',').collect(),
            "BYMONTH" => by_month = v.split(',').collect(),
            // The week's first day changes nothing a person reads.
            "WKST" => {}
            _ => rest.push(part),
        }
    }
    let Some(freq) = freq else {
        return format!("Repeats ({rule})");
    };
    let unit = match freq.to_ascii_uppercase().as_str() {
        "SECONDLY" => ("second", "Every second"),
        "MINUTELY" => ("minute", "Every minute"),
        "HOURLY" => ("hour", "Hourly"),
        "DAILY" => ("day", "Daily"),
        "WEEKLY" => ("week", "Weekly"),
        "MONTHLY" => ("month", "Monthly"),
        "YEARLY" => ("year", "Yearly"),
        _ => return format!("Repeats ({rule})"),
    };
    let mut out = if interval > 1 {
        format!("Every {interval} {}s", unit.0)
    } else {
        unit.1.to_string()
    };

    if !by_day.is_empty() {
        match days(&by_day) {
            Some(d) => out.push_str(&format!(" on {d}")),
            None => rest.push("BYDAY"),
        }
    }
    if !by_month_day.is_empty() {
        let d: Vec<String> = by_month_day.iter().map(|d| month_day(d)).collect();
        out.push_str(&format!(" on {}", join_and(&d)));
    }
    if !by_month.is_empty() {
        let m: Option<Vec<&str>> = by_month.iter().map(|m| month_name(m)).collect();
        match m {
            Some(m) => out.push_str(&format!(" in {}", join_and(&m))),
            None => rest.push("BYMONTH"),
        }
    }
    if let Some(c) = count {
        match c {
            "1" => out.push_str(", once"),
            c => out.push_str(&format!(", {c} times")),
        }
    }
    if let Some(u) = until {
        out.push_str(&format!(", until {}", until_date(u, tz)));
    }
    if !rest.is_empty() {
        out.push_str(&format!(" ({})", rest.join(";")));
    }
    out
}

fn until_date(until: &str, tz: Option<&str>) -> String {
    let Some(t) = EventTime::from_ical(until, None) else {
        return until.to_string();
    };
    let (Some(tz), Ok(instant)) = (
        tz.and_then(|z| z.parse::<chrono_tz::Tz>().ok()),
        t.instant(None),
    ) else {
        return t.display_date();
    };
    if t.is_all_day() {
        return t.display_date();
    }
    instant
        .at
        .with_timezone(&tz)
        .format("%a %-d %b %Y")
        .to_string()
}

/// `MO,TH` → "Monday and Thursday"; `1TU` → "the first Tuesday";
/// `MO,TU,WE,TH,FR` → "weekdays". `None` for a spelling it cannot read.
fn days(by_day: &[&str]) -> Option<String> {
    let mut plain: Vec<&str> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    for d in by_day {
        let d = d.trim();
        let split = d.len().checked_sub(2)?;
        let (ord, code) = d.split_at(split);
        let name = weekday(code)?;
        if ord.is_empty() {
            plain.push(code);
            words.push(name.to_string());
        } else {
            let n: i32 = ord.trim_start_matches('+').parse().ok()?;
            words.push(format!("the {} {name}", ordinal(n)?));
        }
    }
    let mut sorted = plain.clone();
    sorted.sort_unstable();
    if words.len() == 5 && sorted == ["FR", "MO", "TH", "TU", "WE"] {
        return Some("weekdays".to_string());
    }
    Some(join_and(&words))
}

fn weekday(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_uppercase().as_str() {
        "MO" => "Monday",
        "TU" => "Tuesday",
        "WE" => "Wednesday",
        "TH" => "Thursday",
        "FR" => "Friday",
        "SA" => "Saturday",
        "SU" => "Sunday",
        _ => return None,
    })
}

fn ordinal(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        5 => "fifth",
        -1 => "last",
        -2 => "second-to-last",
        _ => return None,
    })
}

fn month_day(d: &str) -> String {
    match d.trim() {
        "-1" => "the last day".to_string(),
        d => format!("day {}", d.trim_start_matches('+')),
    }
}

fn month_name(m: &str) -> Option<&'static str> {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let i: usize = m.trim().parse().ok()?;
    NAMES.get(i.checked_sub(1)?).copied()
}

fn join_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_string(),
        [init @ .., last] => format!(
            "{} and {}",
            init.iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rule shape a real Fastmail account held, measured: weekly
    /// with days and an end, yearly, daily with a count, monthly by
    /// weekday and by day, every-N-weeks.
    #[test]
    fn the_rules_calendars_actually_hold_read_as_sentences() {
        let la = Some("America/Los_Angeles");
        assert_eq!(
            describe("FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=20261231T170000Z", la),
            "Weekly on Monday and Thursday, until Thu 31 Dec 2026"
        );
        // 07:59 UTC on the 1st is still the 31st in Los Angeles.
        assert_eq!(
            describe("FREQ=WEEKLY;BYDAY=FR;UNTIL=20270101T075900Z", la),
            "Weekly on Friday, until Thu 31 Dec 2026"
        );
        assert_eq!(describe("FREQ=YEARLY", None), "Yearly");
        assert_eq!(describe("FREQ=DAILY;COUNT=10", None), "Daily, 10 times");
        assert_eq!(
            describe("FREQ=MONTHLY;COUNT=12;BYDAY=1TU", None),
            "Monthly on the first Tuesday, 12 times"
        );
        assert_eq!(
            describe("FREQ=MONTHLY;BYDAY=-1FR", None),
            "Monthly on the last Friday"
        );
        assert_eq!(
            describe("FREQ=MONTHLY;BYMONTHDAY=15", None),
            "Monthly on day 15"
        );
        assert_eq!(
            describe("FREQ=WEEKLY;INTERVAL=2;BYDAY=TH;WKST=SU", None),
            "Every 2 weeks on Thursday"
        );
        assert_eq!(
            describe("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", None),
            "Weekly on weekdays"
        );
        assert_eq!(
            describe("FREQ=YEARLY;BYMONTH=3;BYDAY=2SU", None),
            "Yearly on the second Sunday in March"
        );
        assert_eq!(
            describe("FREQ=YEARLY;UNTIL=20300101", None),
            "Yearly, until Tue 1 Jan 2030"
        );
    }

    #[test]
    fn what_the_sentence_cannot_say_is_kept_verbatim() {
        assert_eq!(
            describe("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", None),
            "Monthly on weekdays (BYSETPOS=-1)"
        );
        assert_eq!(describe("X=1", None), "Repeats (X=1)");
    }
}
