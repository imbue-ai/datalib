//! The walk of completed tasks, as windows of completion time. Todoist
//! answers for at most three months at a time, so what is left to read
//! — the range wanted less the spans `coverage` holds — is cut into
//! windows of [`WINDOW_DAYS`], newest first. Every stamp here is UTC to
//! the second, `YYYY-MM-DDTHH:MM:SSZ`, so text order is time order.

use datalib_etl_web::coverage::{self, Span};
use datalib_time::IsoOffsetTimestamp;

/// Under Todoist's three months, whatever months it counts.
pub const WINDOW_DAYS: i64 = 84;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `ms` as a stamp of this module's form.
pub fn stamp(ms: i64) -> Option<String> {
    let t = IsoOffsetTimestamp::from_unix_millis(ms - ms.rem_euclid(1000))?;
    Some(t.to_rfc3339_secs().replace("+00:00", "Z"))
}

/// Any stamp Todoist writes (`2015-07-31T18:32:06.000000Z`) or a bare
/// `YYYY-MM-DD`, as this module's form; `None` for neither.
pub fn normalize(s: &str) -> Option<String> {
    let parsed = datalib_time::parse_with_assumed_utc(s)
        .or_else(|_| datalib_time::parse_yyyy_mm_dd_assumed_utc(s))
        .ok()?;
    stamp(parsed.to_unix_millis())
}

fn ms(s: &str) -> Option<i64> {
    datalib_time::parse_strict(s)
        .ok()
        .map(|t| t.to_unix_millis())
}

/// The windows to read, newest first: every gap between `floor` and
/// `now` that `held` does not cover, each cut into windows of at most
/// [`WINDOW_DAYS`] from its top down.
pub fn to_read(floor: &str, now: &str, held: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    for gap in coverage::gaps(&Span::new(floor, now), held)
        .into_iter()
        .rev()
    {
        let (Some(lo), Some(mut hi)) = (ms(&gap.lo), ms(&gap.hi)) else {
            continue;
        };
        while hi > lo {
            let from = (hi - WINDOW_DAYS * DAY_MS).max(lo);
            if let (Some(a), Some(b)) = (stamp(from), stamp(hi)) {
                out.push(Span::new(a, b));
            }
            hi = from;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todoists_stamps_and_a_bare_date_normalize() {
        assert_eq!(
            normalize("2015-07-31T18:32:06.000000Z").as_deref(),
            Some("2015-07-31T18:32:06Z")
        );
        assert_eq!(
            normalize("2024-01-01").as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
        assert_eq!(normalize("stardate 41153.7"), None);
    }

    /// The first run reads from now back to the floor in windows of at
    /// most twelve weeks, newest first, each starting where the one
    /// after it ended.
    #[test]
    fn a_first_walk_is_windows_from_now_down_to_the_floor() {
        let w = to_read("2364-01-01T00:00:00Z", "2364-07-01T00:00:00Z", &[]);
        let spans: Vec<(&str, &str)> = w.iter().map(|s| (s.lo.as_str(), s.hi.as_str())).collect();
        assert_eq!(
            spans,
            [
                ("2364-04-08T00:00:00Z", "2364-07-01T00:00:00Z"),
                ("2364-01-15T00:00:00Z", "2364-04-08T00:00:00Z"),
                ("2364-01-01T00:00:00Z", "2364-01-15T00:00:00Z"),
            ]
        );
    }

    /// A later run reads only from where the held span ends to now; a
    /// run cut short below that also reads what it never reached.
    #[test]
    fn a_later_walk_reads_only_the_gaps() {
        let held = [Span::new("2364-03-01T00:00:00Z", "2364-07-01T00:00:00Z")];
        let w = to_read("2364-01-01T00:00:00Z", "2364-07-02T00:00:00Z", &held);
        let spans: Vec<(&str, &str)> = w.iter().map(|s| (s.lo.as_str(), s.hi.as_str())).collect();
        assert_eq!(
            spans,
            [
                ("2364-07-01T00:00:00Z", "2364-07-02T00:00:00Z"),
                ("2364-01-01T00:00:00Z", "2364-03-01T00:00:00Z"),
            ]
        );
        assert!(
            to_read("2364-01-01T00:00:00Z", "2364-07-01T00:00:00Z", &held[..])
                .iter()
                .all(|s| s.hi.as_str() <= "2364-03-01T00:00:00Z")
        );
    }
}
