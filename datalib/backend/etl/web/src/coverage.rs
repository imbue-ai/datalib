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

use anyhow::{Context, Result};
use sqlx::{Sqlite, Transaction};

/// One table for every range a store's download walks, keyed by a scope
/// the provider names (`activities`, `history:<channel>`). A provider
/// that walks ranges adds this to its store's DDL.
pub const DDL: &str = "CREATE TABLE IF NOT EXISTS coverage (\
    scope TEXT NOT NULL, lo TEXT NOT NULL, hi TEXT NOT NULL, \
    PRIMARY KEY (scope, lo))";

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
