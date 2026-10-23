//! A newest-first walk over one connection, by `updatedAt`, and what it
//! has covered.
//!
//! What is left to walk is every stretch of update times no `coverage`
//! span holds, taken newest first: on a first run that is everything,
//! from now back; on every run after, it is the stretch above what the
//! last run reached, so a run on a quiet day reads one page. A walk
//! that stopped part-way left its span ending where it stopped, and the
//! next run finishes the stretch below after reading what is new.
//!
//! A gap is walked by its bounds rather than by Linear's cursor: each
//! page asks for records updated at or before the oldest stamp the last
//! page held, so a record that moves to the top mid-walk (its
//! `updatedAt` only ever grows) is read by the next run, never skipped.
//! Both bounds are inclusive, so the record at a bound is listed twice
//! and written once. Only a page that is all one stamp falls back to
//! the cursor.

use datalib_etl_web::coverage::{self, Span};
use serde_json::{json, Value};

/// A span's ends are Linear stamps (`2026-10-21T05:38:45.862Z`), which
/// sort as text. These sort below and above every one of them.
pub const START_OF_TIME: &str = "";
pub const END_OF_TIME: &str = "~";

/// What is left to walk, newest first.
pub fn gaps_newest_first(held: &[Span]) -> Vec<Span> {
    let mut gaps = coverage::gaps(&Span::new(START_OF_TIME, END_OF_TIME), held);
    gaps.reverse();
    gaps
}

/// Where a walk of one gap is: the newest stamp it may still ask for,
/// and the cursor when a page of one stamp left it no other way down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub upper: Option<String>,
    pub after: Option<String>,
    /// The top of what this gap's walk has read: the gap's own top, or
    /// in the open gap above everything, the newest stamp it has seen.
    pub top: Option<String>,
}

impl Position {
    pub fn start(gap: &Span) -> Self {
        let upper = (gap.hi != END_OF_TIME).then(|| gap.hi.clone());
        Self {
            top: upper.clone(),
            upper,
            after: None,
        }
    }
}

/// The `updatedAt` filter for one page of `gap` at `pos`.
pub fn bounds(gap: &Span, pos: &Position) -> Option<Value> {
    let mut b = serde_json::Map::new();
    if gap.lo != START_OF_TIME {
        b.insert("gte".into(), json!(gap.lo));
    }
    if let Some(upper) = &pos.upper {
        b.insert("lte".into(), json!(upper));
    }
    (!b.is_empty()).then(|| json!({ "updatedAt": b }))
}

/// `a` and `b`, either of which may be absent.
pub fn and(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    match (a, b) {
        (Some(a), Some(b)) => Some(json!({ "and": [a, b] })),
        (a, b) => a.or(b),
    }
}

/// What one page of a gap came to: the span it lets the walk claim, in
/// the transaction that stores the page, and where to go next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub cover: Option<Span>,
    pub next: Option<Position>,
}

/// `stamps` are the page's `updatedAt`s, newest first.
pub fn step(
    gap: &Span,
    pos: &Position,
    stamps: &[&str],
    has_next: bool,
    end_cursor: Option<String>,
) -> Step {
    let top = pos
        .top
        .clone()
        .or_else(|| stamps.first().map(|s| s.to_string()));
    let oldest = stamps.last().map(|s| s.to_string());
    if !has_next || oldest.is_none() {
        // Read down to the gap's floor.
        return Step {
            cover: top.map(|hi| Span::new(gap.lo.clone(), hi)),
            next: None,
        };
    }
    let oldest = oldest.expect("checked above");
    let cover = top.clone().map(|hi| Span::new(oldest.clone(), hi));
    let next = if pos.upper.as_deref() == Some(oldest.as_str()) {
        Position {
            upper: pos.upper.clone(),
            after: end_cursor,
            top,
        }
    } else {
        Position {
            upper: Some(oldest),
            after: None,
            top,
        }
    };
    Step {
        cover,
        next: Some(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "2026-09-01T00:00:00.000Z";
    const T2: &str = "2026-09-02T00:00:00.000Z";
    const T3: &str = "2026-09-03T00:00:00.000Z";
    const T4: &str = "2026-09-04T00:00:00.000Z";

    fn s(lo: &str, hi: &str) -> Span {
        Span::new(lo, hi)
    }

    #[test]
    fn a_first_run_walks_everything_and_a_later_one_what_is_newer() {
        assert_eq!(gaps_newest_first(&[]), [s(START_OF_TIME, END_OF_TIME)]);
        assert_eq!(
            gaps_newest_first(&[s(START_OF_TIME, T3)]),
            [s(T3, END_OF_TIME)]
        );
    }

    /// The run that stopped after two pages: what is newer comes first,
    /// then the rest of the history under where it stopped.
    #[test]
    fn a_walk_cut_short_finishes_below_after_reading_what_is_new() {
        assert_eq!(
            gaps_newest_first(&[s(T2, T3)]),
            [s(T3, END_OF_TIME), s(START_OF_TIME, T2)]
        );
    }

    #[test]
    fn the_open_gap_asks_from_its_floor_with_no_ceiling() {
        let gap = s(T3, END_OF_TIME);
        let pos = Position::start(&gap);
        assert_eq!(bounds(&gap, &pos), Some(json!({"updatedAt": {"gte": T3}})));
        assert_eq!(bounds(&s(START_OF_TIME, END_OF_TIME), &pos), None);
        assert_eq!(
            and(Some(json!({"a": 1})), bounds(&gap, &pos)),
            Some(json!({"and": [{"a": 1}, {"updatedAt": {"gte": T3}}]}))
        );
    }

    /// Each page claims from its oldest stamp to the newest the walk has
    /// seen, and the next page asks from that oldest stamp down.
    #[test]
    fn each_page_covers_down_to_its_oldest_and_the_next_starts_there() {
        let gap = s(START_OF_TIME, END_OF_TIME);
        let first = step(
            &gap,
            &Position::start(&gap),
            &[T4, T3],
            true,
            Some("c1".into()),
        );
        assert_eq!(first.cover, Some(s(T3, T4)));
        let next = first.next.unwrap();
        assert_eq!(next.upper.as_deref(), Some(T3));
        assert_eq!(next.after, None, "by bound, not by cursor");
        assert_eq!(bounds(&gap, &next), Some(json!({"updatedAt": {"lte": T3}})));

        let last = step(&gap, &next, &[T3, T2, T1], false, None);
        assert_eq!(last.cover, Some(s(START_OF_TIME, T4)), "down to the floor");
        assert_eq!(last.next, None);
    }

    /// A gap between two held spans claims its own top, even when it
    /// turns out to hold nothing.
    #[test]
    fn an_empty_bounded_gap_is_covered_whole() {
        let gap = s(T1, T3);
        let done = step(&gap, &Position::start(&gap), &[], false, None);
        assert_eq!(done.cover, Some(s(T1, T3)));
    }

    /// Nothing newer than the last run: nothing is claimed, since there
    /// is no stamp to claim up to.
    #[test]
    fn an_empty_open_gap_claims_nothing() {
        let gap = s(T3, END_OF_TIME);
        let done = step(&gap, &Position::start(&gap), &[], false, None);
        assert_eq!(done.cover, None);
    }

    /// A page that is all one stamp cannot move the bound down; the
    /// cursor takes it past the tie.
    #[test]
    fn a_page_of_one_stamp_moves_on_by_cursor() {
        let gap = s(START_OF_TIME, END_OF_TIME);
        let pos = Position {
            upper: Some(T2.into()),
            after: None,
            top: Some(T4.into()),
        };
        let tie = step(&gap, &pos, &[T2, T2], true, Some("c7".into()));
        let next = tie.next.unwrap();
        assert_eq!(next.upper.as_deref(), Some(T2));
        assert_eq!(next.after.as_deref(), Some("c7"));
        let after_tie = step(&gap, &next, &[T2, T1], true, Some("c8".into()));
        assert_eq!(after_tie.next.unwrap().after, None, "back to bounds");
    }
}
