//! Which days a run reads: the pure part of the walk. A day is
//! `YYYY-MM-DD` in the store's coverage, and `YYMMDD` as a number on the
//! wire and in an entry's `Pdt`.

use chrono::{Datelike, Duration, NaiveDate};
use datalib_etl_web::coverage::Span;
use serde_json::Value;

/// The `Kid` a day's request names: the one child's id when there is
/// one, else `0` for every child, as the web page asks.
pub fn kid_param(kids: &[Value]) -> String {
    match kids {
        [only] => match only.get("Id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => "0".to_string(),
        },
        _ => "0".to_string(),
    }
}

/// A child's birthday as the home page writes it, `M/D/YYYY`.
pub fn birthday(kid: &Value) -> Option<NaiveDate> {
    let s = kid.get("BDay")?.as_str()?;
    NaiveDate::parse_from_str(s.trim(), "%m/%d/%Y").ok()
}

pub fn day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

pub fn key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// The day as Baby Connect writes it. Its years are two digits, counted
/// from 2000, as the web page's `getTSForDay` counts them.
pub fn pdt(d: NaiveDate) -> u32 {
    let years = (d.year() - 2000).rem_euclid(100) as u32;
    years * 10000 + d.month() * 100 + d.day()
}

/// The days of `[from, to]` the walk owes: every one no held span
/// covers, oldest first.
pub fn owed(from: NaiveDate, to: NaiveDate, held: &[Span]) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = from;
    while d <= to {
        let k = key(d);
        if !held.iter().any(|s| s.lo <= k && k <= s.hi) {
            out.push(d);
        }
        d += Duration::days(1);
    }
    out
}

/// Whether a day read whole may be recorded as read for good. The last
/// two days may still be written to, by a parent who logs the night's
/// feeds the next morning, so they are read again by every run.
pub fn settled(d: NaiveDate, today: NaiveDate) -> bool {
    d < today - Duration::days(1)
}

/// The span that records `d` as read. Spans merge only where they
/// overlap, so when a stored span already covers the day before `d`,
/// the span starts at that day: a run of days stays one span, not a row
/// per day.
pub fn span_for(d: NaiveDate, held: &[Span]) -> Span {
    let eve = key(d - Duration::days(1));
    let lo = if held.iter().any(|s| s.lo <= eve && eve <= s.hi) {
        eve
    } else {
        key(d)
    };
    Span::new(lo, key(d))
}

/// The entries of one day's answer that are that day's: the answer also
/// carries some from the days beside it.
pub fn entries_of(answer: &Value, d: NaiveDate) -> Vec<Value> {
    let want = pdt(d) as u64;
    answer
        .get("list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|e| e.get("Pdt").and_then(Value::as_u64) == Some(want))
        .cloned()
        .collect()
}

/// The people a day's answer names, each with its id: a map of id to
/// person, or a list of people that carry their own.
pub fn users_of(answer: &Value) -> Vec<Value> {
    match answer.get("users") {
        Some(Value::Object(by_id)) => by_id
            .iter()
            .filter_map(|(id, u)| {
                let mut u = u.as_object()?.clone();
                if !u.contains_key("Id") {
                    let id = id
                        .parse::<u64>()
                        .map(Value::from)
                        .unwrap_or_else(|_| Value::from(id.as_str()));
                    u.insert("Id".to_string(), id);
                }
                Some(Value::Object(u))
            })
            .collect(),
        Some(Value::Array(list)) => list.iter().filter(|u| u.is_object()).cloned().collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(s: &str) -> NaiveDate {
        day(s).unwrap()
    }

    #[test]
    fn one_child_is_asked_for_by_id_and_several_all_at_once() {
        let wesley: Value = serde_json::from_str(r#"{"Id": 5629499534213121}"#).unwrap();
        assert_eq!(kid_param(std::slice::from_ref(&wesley)), "5629499534213121");
        assert_eq!(kid_param(&[wesley.clone(), json!({"Id": 7})]), "0");
        assert_eq!(kid_param(&[]), "0");
    }

    #[test]
    fn a_day_is_written_yymmdd() {
        assert_eq!(pdt(d("2031-03-17")), 310317);
        assert_eq!(pdt(d("2008-01-02")), 80102);
        assert_eq!(pdt(d("2368-10-12")), 681012);
        assert_eq!(
            birthday(&json!({"BDay": "3/17/2031"})),
            Some(d("2031-03-17"))
        );
        assert_eq!(birthday(&json!({"BDay": ""})), None);
    }

    #[test]
    fn only_days_no_span_covers_are_owed() {
        let held = vec![Span::new("2031-03-02", "2031-03-03")];
        assert_eq!(
            owed(d("2031-03-01"), d("2031-03-05"), &held),
            vec![d("2031-03-01"), d("2031-03-04"), d("2031-03-05")]
        );
        assert!(owed(d("2031-03-05"), d("2031-03-01"), &[]).is_empty());
    }

    #[test]
    fn a_day_after_a_held_one_extends_its_span() {
        let held = vec![Span::new("2031-03-01", "2031-03-03")];
        assert_eq!(
            span_for(d("2031-03-04"), &held),
            Span::new("2031-03-03", "2031-03-04")
        );
        assert_eq!(
            span_for(d("2031-03-06"), &held),
            Span::new("2031-03-06", "2031-03-06")
        );
    }

    #[test]
    fn the_last_two_days_are_never_settled() {
        let today = d("2031-03-10");
        assert!(settled(d("2031-03-08"), today));
        assert!(!settled(d("2031-03-09"), today));
        assert!(!settled(today, today));
    }

    #[test]
    fn a_days_answer_keeps_only_its_own_entries() {
        let answer = json!({"list": [
            {"Id": 1, "Pdt": 310317}, {"Id": 2, "Pdt": 140518}, {"Id": 3, "Pdt": 310317}
        ]});
        let ids: Vec<i64> = entries_of(&answer, d("2031-03-17"))
            .iter()
            .map(|e| e["Id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn people_keyed_by_id_carry_their_id() {
        let answer = json!({"users": {"5066549580791808": {"Name": "Beverly"}}});
        let users = users_of(&answer);
        assert_eq!(users[0]["Id"].to_string(), "5066549580791808");
        assert_eq!(users[0]["Name"], "Beverly");
        assert_eq!(users_of(&json!({"users": [{"Id": 7}]})).len(), 1);
        assert!(users_of(&json!({})).is_empty());
    }
}
