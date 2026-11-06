//! Which stretches of a range a download has looked at.
//!
//! "Which part of this channel's history have I not read?" cannot be
//! worked out from the messages: a stretch with nothing in it and a
//! stretch never asked for look the same. So a walk over a range records
//! the spans it covered, in the transaction that stores what it found
//! there, and what is left to walk is the range wanted minus the spans
//! held (docs/dev/data_architecture_ingestion.md, "What is left to fetch"). A newest stored row is never
//! read as "fetched up to here".
//!
//! A span's ends are strings that sort the way the range does: ISO
//! dates, fixed-width timestamps. Both ends are inside the span.
//!
//! Where a walk's top is the newest thing it saw, the top says nothing
//! about when anyone last looked. A provider whose scopes can fall behind
//! one another records that apart, in `coverage_checked`.
//!
//! A walk newest first that upstream cuts off asks again below the oldest
//! thing it was given. [`settled`] and [`next_below`] are that decision;
//! the requests and the paging stay the walk's.

use anyhow::{Context, Result};
use sqlx::{Sqlite, Transaction};

/// One table for every range a store's download walks, keyed by a scope
/// the provider names (`activities`, `history:<channel>`). A provider
/// that walks ranges adds this to its store's DDL.
pub const DDL: &str = "CREATE TABLE IF NOT EXISTS coverage (\
    scope TEXT NOT NULL, lo TEXT NOT NULL, hi TEXT NOT NULL, \
    PRIMARY KEY (scope, lo))";

/// When upstream last had nothing in a scope newer than what its spans
/// hold. Narrow and on its own because it moves every run: a wider row
/// would be rewritten whole each time (docs/dev/doltlite.md, "What a
/// write costs").
pub const CHECKED_DDL: &str = "CREATE TABLE IF NOT EXISTS coverage_checked (\
    scope TEXT NOT NULL PRIMARY KEY, checked_at_utc TEXT NOT NULL, tz_offset TEXT)";

/// Record that, at `at`, upstream had nothing newer in each of `scopes`.
/// Call it in the transaction that stores the end of the walk that found
/// that out, and with a time from before the walk began.
pub async fn checked(
    tx: &mut Transaction<'_, Sqlite>,
    scopes: &[String],
    at: &datalib_time::IsoOffsetTimestamp,
) -> Result<()> {
    if scopes.is_empty() {
        return Ok(());
    }
    let (at_utc, tz_offset) = at.to_utc_and_offset();
    let scopes = serde_json::to_string(scopes).context("serialize the scopes checked")?;
    sqlx::query(
        "INSERT INTO coverage_checked (scope, checked_at_utc, tz_offset) \
         SELECT value, ?, ? FROM json_each(?) WHERE true \
         ON CONFLICT(scope) DO UPDATE SET checked_at_utc = excluded.checked_at_utc, \
            tz_offset = excluded.tz_offset",
    )
    .bind(&at_utc)
    .bind(&tz_offset)
    .bind(&scopes)
    .execute(&mut **tx)
    .await
    .context("record when coverage was last checked")?;
    Ok(())
}

/// A stretch of a range, both ends included.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub lo: String,
    pub hi: String,
}

impl Span {
    pub fn new(lo: impl Into<String>, hi: impl Into<String>) -> Self {
        Self {
            lo: lo.into(),
            hi: hi.into(),
        }
    }
}

/// The parts of `want` no span of `held` covers, lowest first. A gap's
/// ends are the ends of the spans beside it, so they are points already
/// looked at: a walk may ask for them again or leave them out.
pub fn gaps(want: &Span, held: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    if want.lo > want.hi {
        return out;
    }
    // Everything below `from` is accounted for; `from` itself is only
    // once a held span has been seen to reach it.
    let mut from = want.lo.clone();
    let mut from_is_held = false;
    for span in merged(held.to_vec()) {
        if span.hi < from {
            continue;
        }
        if span.lo > want.hi {
            break;
        }
        if span.lo > from {
            out.push(Span::new(from, span.lo));
        }
        from = span.hi;
        from_is_held = true;
        if from >= want.hi {
            return out;
        }
    }
    if from < want.hi || !from_is_held {
        out.push(Span::new(from, want.hi.clone()));
    }
    out
}

/// `spans` with every two that touch or overlap made one, lowest first.
pub fn merged(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|s| s.lo <= s.hi);
    spans.sort();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if span.lo <= last.hi => {
                if span.hi > last.hi {
                    last.hi = span.hi;
                }
            }
            _ => out.push(span),
        }
    }
    out
}

/// The part of `asked` one answer of a newest-first walk settles: from
/// `top`, where the walk began, down to the oldest thing the answer
/// held, or down to the bottom of `asked` when it was not `cut_off`. A
/// walk with no `top` has seen nothing to claim up to, and an answer cut
/// off with no `oldest` says nothing of where it stopped.
pub fn settled(
    asked: &Span,
    top: Option<&str>,
    oldest: Option<&str>,
    cut_off: bool,
) -> Option<Span> {
    let hi = top?;
    let lo = if cut_off {
        oldest?.max(asked.lo.as_str())
    } else {
        asked.lo.as_str()
    };
    (lo <= hi).then(|| Span::new(lo, hi))
}

/// What a newest-first walk still has to ask after one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Nothing: the answer was whole, or reached the bottom of what was
    /// asked.
    Done,
    /// This much, whose top is the oldest thing the answer held. The top
    /// is asked for again, since others may share its stamp, and it is
    /// below the top asked for last time, so a walk that follows this
    /// ends.
    Below(Span),
    /// The answer was cut off and asking below it would get no further:
    /// its oldest is no older than the top that was asked for, or it has
    /// no oldest. What to do then is the walk's to decide.
    Stuck,
}

pub fn next_below(asked: &Span, oldest: Option<&str>, cut_off: bool) -> Next {
    if !cut_off {
        return Next::Done;
    }
    match oldest {
        None => Next::Stuck,
        Some(oldest) if oldest >= asked.hi.as_str() => Next::Stuck,
        Some(oldest) if oldest <= asked.lo.as_str() => Next::Done,
        Some(oldest) => Next::Below(Span::new(asked.lo.as_str(), oldest)),
    }
}

/// The spans of `scope` this store holds, lowest first.
pub async fn held<'e, E>(executor: E, scope: &str) -> Result<Vec<Span>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT lo, hi FROM coverage WHERE scope = ? ORDER BY lo")
            .bind(scope)
            .fetch_all(executor)
            .await
            .with_context(|| format!("read the coverage of {scope}"))?;
    Ok(rows.into_iter().map(|(lo, hi)| Span { lo, hi }).collect())
}

/// Record that `span` of `scope` has been looked at. Call it in the
/// transaction that stores what was found there, and only for a stretch
/// the walk read every page of: a span covers its whole length.
pub async fn cover(tx: &mut Transaction<'_, Sqlite>, scope: &str, span: Span) -> Result<()> {
    let mut all = held(&mut **tx, scope).await?;
    all.push(span);
    let all = merged(all);
    sqlx::query("DELETE FROM coverage WHERE scope = ?")
        .bind(scope)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("rewrite the coverage of {scope}"))?;
    for span in all {
        sqlx::query("INSERT INTO coverage (scope, lo, hi) VALUES (?, ?, ?)")
            .bind(scope)
            .bind(&span.lo)
            .bind(&span.hi)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("record the coverage of {scope}"))?;
    }
    Ok(())
}

/// Forget every span of `scope`: a channel that is gone, a source reset.
pub async fn forget(tx: &mut Transaction<'_, Sqlite>, scope: &str) -> Result<()> {
    sqlx::query("DELETE FROM coverage WHERE scope = ?")
        .bind(scope)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget the coverage of {scope}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(lo: &str, hi: &str) -> Span {
        Span::new(lo, hi)
    }

    #[test]
    fn nothing_held_leaves_the_whole_range() {
        assert_eq!(gaps(&s("10", "90"), &[]), [s("10", "90")]);
    }

    #[test]
    fn a_range_held_whole_leaves_nothing() {
        assert_eq!(gaps(&s("10", "90"), &[s("10", "90")]), []);
        assert_eq!(gaps(&s("20", "80"), &[s("10", "90")]), []);
        assert_eq!(gaps(&s("10", "90"), &[s("10", "50"), s("50", "90")]), []);
    }

    /// The Slack bug: one page of the newest messages was stored and the
    /// walk died. What was read is the top; everything under it is still
    /// to walk, and so is whatever arrives above it.
    #[test]
    fn a_stretch_read_from_the_top_leaves_what_is_under_and_over_it() {
        assert_eq!(
            gaps(&s("10", "99"), &[s("70", "90")]),
            [s("10", "70"), s("90", "99")]
        );
    }

    #[test]
    fn gaps_between_spans_are_named_by_the_spans_beside_them() {
        assert_eq!(
            gaps(&s("10", "90"), &[s("20", "30"), s("50", "60")]),
            [s("10", "20"), s("30", "50"), s("60", "90")]
        );
    }

    /// A widened `since` is a gap below what is held, with no record of
    /// the config the old walk ran under.
    #[test]
    fn a_range_widened_downward_owes_only_the_new_stretch() {
        assert_eq!(gaps(&s("05", "90"), &[s("10", "90")]), [s("05", "10")]);
    }

    #[test]
    fn spans_outside_the_range_do_not_count() {
        assert_eq!(
            gaps(&s("40", "60"), &[s("10", "20"), s("70", "80")]),
            [s("40", "60")]
        );
        assert_eq!(gaps(&s("40", "60"), &[s("10", "50")]), [s("50", "60")]);
    }

    #[test]
    fn a_single_point_is_held_or_it_is_not() {
        assert_eq!(gaps(&s("50", "50"), &[s("10", "90")]), []);
        assert_eq!(gaps(&s("50", "50"), &[s("60", "90")]), [s("50", "50")]);
        assert_eq!(gaps(&s("50", "50"), &[]), [s("50", "50")]);
    }

    #[test]
    fn touching_and_overlapping_spans_become_one() {
        assert_eq!(
            merged(vec![
                s("50", "60"),
                s("10", "30"),
                s("30", "40"),
                s("55", "70")
            ]),
            [s("10", "40"), s("50", "70")]
        );
    }

    #[test]
    fn an_answer_settles_from_the_top_down_to_where_it_stopped() {
        let asked = s("20", "90");
        assert_eq!(
            settled(&asked, Some("90"), Some("70"), true),
            Some(s("70", "90")),
            "cut off: down to its oldest"
        );
        assert_eq!(
            settled(&asked, Some("90"), Some("70"), false),
            Some(s("20", "90")),
            "whole: down to the bottom, however old its oldest"
        );
        assert_eq!(
            settled(&asked, Some("90"), None, false),
            Some(s("20", "90")),
            "whole and empty looked at everything"
        );
        assert_eq!(
            settled(&asked, Some("90"), Some("10"), true),
            Some(s("20", "90")),
            "a result under what was asked says nothing of what is under it"
        );
        assert_eq!(
            settled(&asked, Some("90"), None, true),
            None,
            "cut off with nothing to say where"
        );
        assert_eq!(settled(&asked, None, Some("70"), true), None, "no top");
        assert_eq!(
            settled(&asked, Some("10"), None, false),
            None,
            "a top under the bottom is no span"
        );
    }

    #[test]
    fn a_cut_off_answer_leaves_what_is_below_its_oldest() {
        let asked = s("20", "90");
        for oldest in [None, Some("70")] {
            assert_eq!(next_below(&asked, oldest, false), Next::Done, "whole");
        }
        assert_eq!(
            next_below(&asked, Some("70"), true),
            Next::Below(s("20", "70"))
        );
        for reached in ["20", "10"] {
            assert_eq!(
                next_below(&asked, Some(reached), true),
                Next::Done,
                "reached the bottom at {reached}"
            );
        }
        for no_further in [None, Some("90"), Some("95")] {
            assert_eq!(
                next_below(&asked, no_further, true),
                Next::Stuck,
                "{no_further:?}"
            );
        }
        assert_eq!(
            next_below(&s("50", "50"), Some("50"), true),
            Next::Stuck,
            "a gap of one stamp, still cut off, did not reach its bottom"
        );
    }

    /// A walk over stamps newest first, `cap` results to an answer,
    /// following what the two functions say: the stamps it listed, what
    /// it held at the end, how many answers it took, and whether it got
    /// stuck.
    fn walk(stamps: &[&str], cap: usize, gap: &Span) -> (Vec<String>, Vec<Span>, usize, bool) {
        let top = gap.hi.clone();
        let (mut asked, mut listed, mut held) = (gap.clone(), Vec::new(), Vec::new());
        for answers in 1..=stamps.len() + 2 {
            let mut hits: Vec<&str> = stamps
                .iter()
                .copied()
                .filter(|t| *t >= asked.lo.as_str() && *t <= asked.hi.as_str())
                .collect();
            hits.sort();
            hits.reverse();
            let cut_off = hits.len() > cap;
            hits.truncate(cap);
            let oldest = hits.last().copied();
            listed.extend(hits.iter().map(|t| t.to_string()));
            held.extend(settled(&asked, Some(&top), oldest, cut_off));
            match next_below(&asked, oldest, cut_off) {
                Next::Done => return (listed, merged(held), answers, false),
                Next::Stuck => return (listed, merged(held), answers, true),
                Next::Below(rest) => {
                    assert!(rest.hi < asked.hi, "the top must fall: {rest:?}");
                    asked = rest;
                }
            }
        }
        panic!("a walk of {} stamps did not end", stamps.len());
    }

    /// However the stamps fall, a walk lists every one of them, settles
    /// the whole gap, and ends, as long as fewer share one stamp than an
    /// answer holds.
    #[test]
    fn a_walk_lists_everything_and_ends() {
        let gap = s("", "99");
        for (stamps, cap) in [
            (vec![], 2),
            (vec!["10"], 2),
            (vec!["10", "20"], 2),
            (vec!["10", "20", "30", "40", "50"], 2),
            (vec!["10", "20", "20", "30", "30", "40"], 3),
            (vec!["10", "20", "20", "20", "30"], 4),
        ] {
            let (listed, held, _, stuck) = walk(&stamps, cap, &gap);
            assert!(!stuck, "{stamps:?}");
            assert_eq!(held, std::slice::from_ref(&gap), "{stamps:?}");
            for stamp in &stamps {
                assert!(listed.iter().any(|t| t == stamp), "{stamp} of {stamps:?}");
            }
        }
    }

    /// As many share one stamp as an answer holds, and the top is asked
    /// for again: the walk ends at that stamp, holding what is above it
    /// and not what is below.
    #[test]
    fn a_tie_wider_than_an_answer_ends_the_walk_stuck() {
        let (listed, held, answers, stuck) = walk(&["10", "20", "20", "20", "30"], 2, &s("", "99"));
        assert!(stuck);
        assert_eq!(answers, 2, "one reaching the tie, and one that is all tie");
        assert_eq!(held, [s("20", "99")]);
        assert!(!listed.iter().any(|t| t == "10"));
    }

    #[tokio::test]
    async fn a_span_is_recorded_with_the_rows_it_covers_and_merges_with_its_neighbours() {
        let d = tempfile::tempdir().unwrap();
        let pool = datalib_etl::doltlite_raw::open(&d.path().join("c.doltlite_db"), &[DDL])
            .await
            .unwrap();
        for (scope, lo, hi) in [
            ("history:bridge", "70", "90"),
            ("history:bridge", "40", "70"),
            ("history:ten_forward", "10", "20"),
        ] {
            let mut tx = pool.begin().await.unwrap();
            cover(&mut tx, scope, s(lo, hi)).await.unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(
            held(&pool, "history:bridge").await.unwrap(),
            [s("40", "90")]
        );

        // A transaction that does not commit leaves no claim behind.
        let mut tx = pool.begin().await.unwrap();
        cover(&mut tx, "history:bridge", s("10", "40"))
            .await
            .unwrap();
        drop(tx);
        assert_eq!(
            held(&pool, "history:bridge").await.unwrap(),
            [s("40", "90")]
        );

        let mut tx = pool.begin().await.unwrap();
        forget(&mut tx, "history:ten_forward").await.unwrap();
        tx.commit().await.unwrap();
        assert!(held(&pool, "history:ten_forward").await.unwrap().is_empty());
        pool.close().await;
    }
}
