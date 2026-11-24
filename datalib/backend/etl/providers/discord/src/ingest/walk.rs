//! What is left to read of one conversation, and what each page of it
//! settles: pure decisions over message ids. A conversation's messages
//! come newest first, so a walk goes down each stretch it owes, newest
//! stretch first.

use std::collections::VecDeque;

use datalib_etl_web::coverage::{gaps, Span};

use super::snowflake;

/// A stretch of one conversation to read, both ends included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stretch {
    pub lo: u64,
    pub hi: u64,
    /// Nothing newer is held, so the first page is asked for with no top
    /// and whatever came since the listing comes with it.
    pub open_top: bool,
    /// A re-read: a stored message it does not return is gone.
    pub reread: bool,
}

/// One conversation's walk: the stretches left, and where the next page
/// of the first one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    pub channel_id: String,
    stretches: VecDeque<Stretch>,
    /// The oldest id read of the current stretch; the next page is what
    /// is older.
    before: Option<u64>,
    /// How far down the current stretch is settled.
    settled: Option<u64>,
}

/// What a page of messages settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The stretch read whole, as `coverage` takes it.
    pub span: Span,
    /// Where a stored message this page lacks is gone: from `0` up to,
    /// not including, `1` (no top on a first page).
    pub reread: Option<(u64, Option<u64>)>,
    /// The re-read ended with this page.
    pub reread_done: bool,
    /// Messages older than this are outside what was asked for.
    pub floor: u64,
}

/// The stretches owed of a conversation whose newest message is `tip`:
/// what `held` does not cover from `floor` up, and, with `reread_from`,
/// everything from there up once more.
pub fn owed(floor: u64, tip: u64, held: &[Span], reread_from: Option<u64>) -> Vec<Stretch> {
    if tip < floor {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut top = tip;
    if let Some(from) = reread_from {
        let lo = from.max(floor).min(tip);
        out.push(Stretch {
            lo,
            hi: tip,
            open_top: true,
            reread: true,
        });
        if lo <= floor {
            return out;
        }
        top = lo;
    }
    let want = Span::new(snowflake::key(floor), snowflake::key(top));
    for gap in gaps(&want, held).into_iter().rev() {
        let (Some(lo), Some(hi)) = (snowflake::from_key(&gap.lo), snowflake::from_key(&gap.hi))
        else {
            continue;
        };
        out.push(Stretch {
            lo,
            hi,
            open_top: reread_from.is_none() && hi == tip,
            reread: false,
        });
    }
    out
}

impl Walk {
    pub fn new(channel_id: &str, stretches: Vec<Stretch>) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            stretches: stretches.into(),
            before: None,
            settled: None,
        }
    }

    /// The newest message the next page can hold: what walks are ordered
    /// by. `None` when nothing is left.
    pub fn next_key(&self) -> Option<u64> {
        let s = self.stretches.front()?;
        Some(self.before.unwrap_or(s.hi))
    }

    /// The `before` the next page is asked for with; `None` for the
    /// newest messages there are.
    pub fn next_before(&self) -> Option<u64> {
        let s = self.stretches.front()?;
        match self.before {
            Some(b) => Some(b),
            None if s.open_top => None,
            // A gap's top is the end of a stretch already read.
            None => Some(s.hi),
        }
    }

    /// A page came back holding `ids`. `None` when nothing was owed.
    pub fn page(&mut self, ids: &[u64]) -> Option<Settled> {
        let s = self.stretches.front()?.clone();
        let newest = ids.iter().copied().max();
        let oldest = ids.iter().copied().min();
        let top = self
            .settled
            .unwrap_or_else(|| newest.map_or(s.hi, |n| n.max(s.hi)));
        let bottom = match oldest {
            Some(oldest) if oldest > s.lo => oldest,
            // An empty page: nothing older is there at all, so what is
            // left below is settled too.
            None => self.stretches.back().map_or(s.lo, |last| last.lo),
            Some(_) => s.lo,
        };
        let done = bottom <= s.lo;
        let settled = Settled {
            span: Span::new(snowflake::key(bottom), snowflake::key(top)),
            reread: s.reread.then_some((bottom.max(s.lo), self.before)),
            reread_done: s.reread && done,
            floor: s.lo,
        };
        if oldest.is_none() {
            self.stretches.clear();
        } else if done {
            self.stretches.pop_front();
            self.settled = None;
            // The stretch below starts where this one ended, so it goes on
            // from the oldest message read of this one.
            let adjoins = self.stretches.front().is_some_and(|next| next.hi == s.lo);
            let above = ids.iter().copied().filter(|id| *id > s.lo).min();
            self.before = if adjoins { above.or(self.before) } else { None };
        } else {
            self.before = Some(bottom);
            self.settled = Some(bottom);
        }
        Some(settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(lo: u64, hi: u64) -> Span {
        Span::new(snowflake::key(lo), snowflake::key(hi))
    }

    #[test]
    fn a_conversation_never_read_is_one_stretch_open_at_the_top() {
        let got = owed(10, 90, &[], None);
        assert_eq!(
            got,
            [Stretch {
                lo: 10,
                hi: 90,
                open_top: true,
                reread: false
            }]
        );
    }

    /// The quiet conversation: its newest message is the one held, so it
    /// costs nothing.
    #[test]
    fn one_with_nothing_newer_owes_nothing() {
        assert_eq!(owed(10, 90, &[span(10, 90)], None), []);
        assert_eq!(owed(91, 90, &[], None), [], "all older than `since`");
    }

    #[test]
    fn a_walk_cut_off_owes_the_top_and_the_bottom_newest_first() {
        let got = owed(10, 99, &[span(50, 90)], None);
        let ends: Vec<(u64, u64, bool)> = got.iter().map(|s| (s.lo, s.hi, s.open_top)).collect();
        assert_eq!(ends, [(90, 99, true), (10, 50, false)]);
    }

    #[test]
    fn a_reread_takes_its_window_whole_and_the_gaps_under_it() {
        let got = owed(10, 99, &[span(50, 90)], Some(70));
        let ends: Vec<(u64, u64, bool)> = got.iter().map(|s| (s.lo, s.hi, s.reread)).collect();
        assert_eq!(ends, [(70, 99, true), (10, 50, false)]);
    }

    /// Each page settles down to its oldest message; the last reaches the
    /// bottom of the stretch, whether it is past it or empty.
    #[test]
    fn pages_settle_the_stretch_from_the_top_down() {
        let mut w = Walk::new("c", owed(10, 90, &[], None));
        assert_eq!((w.next_before(), w.next_key()), (None, Some(90)));
        // A message came in after the listing named 90 the newest.
        let s = w.page(&[95, 90, 60]).unwrap();
        assert_eq!(s.span, span(60, 95));
        assert_eq!((w.next_before(), w.next_key()), (Some(60), Some(60)));
        let s = w.page(&[50, 40]).unwrap();
        assert_eq!(s.span, span(40, 60));
        let s = w.page(&[]).unwrap();
        assert_eq!(s.span, span(10, 40));
        assert_eq!(w.next_key(), None);
    }

    /// The newest message listed was deleted: what is above the newest
    /// one that came back is settled all the same, so it is not asked
    /// for again.
    #[test]
    fn a_deleted_newest_message_is_settled_by_the_page_that_lacks_it() {
        let mut w = Walk::new("c", owed(10, 90, &[span(10, 60)], None));
        let s = w.page(&[60, 55]).unwrap();
        assert_eq!(s.span, span(60, 90));
        assert_eq!(w.next_key(), None);
    }

    /// An empty page says there is nothing older anywhere.
    #[test]
    fn an_empty_page_settles_every_stretch_below_it() {
        let mut w = Walk::new("c", owed(10, 90, &[span(40, 50)], Some(70)));
        let s = w.page(&[]).unwrap();
        assert_eq!(s.span, span(10, 90));
        assert_eq!(s.reread, Some((70, None)), "only the window is judged");
        assert_eq!(w.next_key(), None);
    }

    /// The stretch under a window that nothing held reaches goes on from
    /// the window's oldest message, not from the window's edge.
    #[test]
    fn an_adjoining_stretch_goes_on_where_the_one_above_ended() {
        let mut w = Walk::new("c", owed(10, 90, &[span(80, 90)], Some(70)));
        w.page(&[90, 75, 60]).unwrap();
        assert_eq!(w.next_before(), Some(75));
        let s = w.page(&[60, 5]).unwrap();
        assert_eq!((s.span, s.reread), (span(10, 70), None));
    }

    #[test]
    fn a_gap_below_held_messages_starts_at_its_top() {
        let mut w = Walk::new("c", owed(10, 90, &[span(50, 90)], None));
        assert_eq!(w.next_before(), Some(50));
        let s = w.page(&[30, 5]).unwrap();
        assert_eq!((s.span, s.floor), (span(10, 50), 10));
        assert_eq!(w.next_key(), None);
    }

    /// A re-read's first page judges everything above its oldest message,
    /// and each later one only what lies under the page before.
    #[test]
    fn a_reread_judges_each_stretch_once() {
        let mut w = Walk::new("c", owed(10, 90, &[span(10, 90)], Some(40)));
        let s = w.page(&[90, 70]).unwrap();
        assert_eq!((s.reread, s.reread_done), (Some((70, None)), false));
        let s = w.page(&[60, 30]).unwrap();
        assert_eq!((s.reread, s.reread_done), (Some((40, Some(70))), true));
        assert_eq!(w.next_key(), None);
    }
}
