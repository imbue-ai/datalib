//! Which days a listing asks Zoom about this run, and in what windows.
//!
//! Zoom lists recordings, reports and summaries by date, a month at most
//! per request. What a listing has read is kept as `coverage` spans of
//! `YYYY-MM-DD` dates, both ends included. A run reads every gap in what
//! is wanted, and the newest `refresh_days` again whatever the coverage
//! says, since a transcript arrives after its meeting and a recording
//! can be deleted. Neighbouring windows share their end day, so the
//! spans they cover touch and merge, and no one-day gap is left between
//! them to be read again on every run.

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use datalib_etl_web::coverage::{self, Span};

/// The most days one request covers: under the shortest month, so no
/// reading of Zoom's "a month at most" refuses it.
pub const WINDOW_DAYS: i64 = 28;

pub fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("{s:?} is not a date"))
}

/// The stretches to read: every gap in `want` that `held` leaves, and
/// the days from `refresh_from` to the end of `want`, merged.
pub fn to_walk(want: &Span, held: &[Span], refresh_from: NaiveDate) -> Result<Vec<Span>> {
    if want.lo > want.hi {
        return Ok(Vec::new());
    }
    let mut spans = coverage::gaps(want, held);
    let refresh = ymd(refresh_from).max(want.lo.clone());
    if refresh <= want.hi {
        spans.push(Span::new(refresh, want.hi.clone()));
    }
    Ok(coverage::merged(spans))
}

/// `span` in windows of at most [`WINDOW_DAYS`] days, newest first, so
/// the meetings a person most likely wants land first. Each window
/// starts on the day the one before it (in time) ends.
pub fn windows(span: &Span) -> Result<Vec<Span>> {
    let (lo, hi) = (date(&span.lo)?, date(&span.hi)?);
    let mut out = Vec::new();
    let mut end = hi;
    loop {
        let start = (end - Duration::days(WINDOW_DAYS - 1)).max(lo);
        out.push(Span::new(ymd(start), ymd(end)));
        if start <= lo {
            return Ok(out);
        }
        end = start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        date(s).unwrap()
    }

    fn s(lo: &str, hi: &str) -> Span {
        Span::new(lo, hi)
    }

    #[test]
    fn a_first_run_reads_everything_wanted() {
        let want = s("2026-01-01", "2026-10-20");
        assert_eq!(
            to_walk(&want, &[], d("2026-09-20")).unwrap(),
            [s("2026-01-01", "2026-10-20")]
        );
    }

    /// The newest days are read again even though they are covered.
    #[test]
    fn a_later_run_reads_only_the_refresh_window() {
        let want = s("2026-01-01", "2026-10-20");
        let held = [s("2026-01-01", "2026-10-19")];
        assert_eq!(
            to_walk(&want, &held, d("2026-09-20")).unwrap(),
            [s("2026-09-20", "2026-10-20")]
        );
    }

    /// An earlier `since` is a gap under what is held, read as well as
    /// the refresh window and not the stretch between.
    #[test]
    fn a_widened_since_reads_the_new_stretch_too() {
        let want = s("2025-01-01", "2026-10-20");
        let held = [s("2026-01-01", "2026-10-20")];
        assert_eq!(
            to_walk(&want, &held, d("2026-09-20")).unwrap(),
            [s("2025-01-01", "2026-01-01"), s("2026-09-20", "2026-10-20")]
        );
    }

    #[test]
    fn a_refresh_window_older_than_since_starts_at_since() {
        let want = s("2026-10-10", "2026-10-20");
        let held = [want.clone()];
        assert_eq!(
            to_walk(&want, &held, d("2026-09-20")).unwrap(),
            [s("2026-10-10", "2026-10-20")]
        );
    }

    #[test]
    fn windows_are_newest_first_and_share_their_ends() {
        let got = windows(&s("2026-09-01", "2026-10-20")).unwrap();
        assert_eq!(
            got,
            [s("2026-09-23", "2026-10-20"), s("2026-09-01", "2026-09-23")]
        );
        let merged = coverage::merged(got);
        assert_eq!(merged, [s("2026-09-01", "2026-10-20")]);
    }

    #[test]
    fn a_one_day_span_is_one_window() {
        assert_eq!(
            windows(&s("2026-10-20", "2026-10-20")).unwrap(),
            [s("2026-10-20", "2026-10-20")]
        );
    }
}
