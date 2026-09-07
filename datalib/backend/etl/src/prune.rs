//! Deleting rows because upstream stopped serving them.
//!
//! A downloader that only ever upserts cannot tell "deleted" from "not
//! mentioned this time", so every prune here rests on the caller having
//! *re-enumerated* something completely: a PR's whole comment list, a
//! channel's history over a bounded time window, an account's whole
//! conversation index. What belongs in this file is the part that is the
//! same every time — the SQL, and the judgement about when a proposed
//! deletion is too large to believe.
//!
//! What does not belong here is the decision that an enumeration *was*
//! complete. That is provider knowledge and stays in the provider.

use std::collections::HashSet;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// Delete the rows of `table` inside `scope` whose id is not in `keep`,
/// and their bookkeeping sidecars. Returns the ids that went.
///
/// `scope` is the bound the caller re-enumerated, as `(column, value)`
/// equality pairs — `[("repo_full_name", repo), ("pr_number", num)]` for one
/// PR's comments. An empty `scope` means the whole table, which is only
/// right when the caller enumerated the whole upstream collection.
///
/// Table and column names are interpolated; callers pass trusted
/// identifiers. Values are bound.
pub async fn prune_scope(
    pool: &SqlitePool,
    table: &str,
    scope: &[(&str, &str)],
    keep: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut where_sql = String::new();
    for (i, (col, _)) in scope.iter().enumerate() {
        where_sql.push_str(if i == 0 { " WHERE " } else { " AND " });
        where_sql.push_str(col);
        where_sql.push_str(" = ?");
    }

    let select = format!("SELECT id FROM {table}{where_sql}");
    // Audited: `table` and every `col` are `&'static str` at each callsite;
    // the scope values are bound below.
    let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(select));
    for (_, val) in scope {
        q = q.bind((*val).to_string());
    }
    let present: Vec<String> = q
        .fetch_all(pool)
        .await
        .with_context(|| format!("list {table} ids in scope for prune"))?;

    let gone: Vec<String> = present
        .into_iter()
        .filter(|id| !keep.contains(id))
        .collect();
    if gone.is_empty() {
        return Ok(gone);
    }

    let mut tx = pool.begin().await.context("begin prune tx")?;
    for chunk in gone.chunks(crate::bulk::SQL_CHUNK) {
        let mut placeholders = String::new();
        crate::bulk::push_placeholder_list(&mut placeholders, chunk.len());
        for sql in [
            format!("DELETE FROM {table} WHERE id IN ({placeholders})"),
            format!("DELETE FROM {table}_bookkeeping WHERE id IN ({placeholders})"),
        ] {
            // Audited: `table` is a `&'static str`; the IN-list is a `?,?,?`
            // run sized from the chunk and every id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id.clone());
            }
            q.execute(&mut *tx)
                .await
                .with_context(|| format!("prune {table}"))?;
        }
    }
    tx.commit().await.context("commit prune tx")?;
    Ok(gone)
}

/// How much of a collection one run is allowed to delete before we treat
/// the enumeration itself as the more likely explanation.
///
/// The failure this exists for is not a provider deleting your data — it is
/// *our* enumeration silently narrowing: an undocumented page cap appears, a
/// filter param changes meaning, an auth downgrade starts returning only
/// public items. Every one of those looks exactly like "almost everything
/// was deleted", and acting on it removes a mirror the user keeps precisely
/// because the provider's copy is not under their control.
///
/// Refusing costs a stale row until someone looks. Proceeding costs the
/// archive. The asymmetry is the whole argument, and it is why this is a
/// hard stop rather than a warning.
#[derive(Debug, Clone, Copy)]
pub struct PruneLimit {
    /// Never refuse a prune of at most this many rows, whatever the
    /// fraction. Deleting three of your four conversations is an ordinary
    /// afternoon; the percentage rule alone would block it forever.
    pub always_allow_up_to: usize,
    /// Above that, refuse when the prune would take more than this share of
    /// what we hold.
    pub max_fraction: f64,
}

impl Default for PruneLimit {
    fn default() -> Self {
        Self {
            always_allow_up_to: 10,
            max_fraction: 0.5,
        }
    }
}

/// What [`PruneLimit::check`] decided, so the caller can log it and carry a
/// count into its run summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneVerdict {
    /// Delete them.
    Proceed,
    /// Delete nothing, and say why. The `reason` is user-facing.
    Refuse { reason: String },
}

impl PruneLimit {
    /// `held` is what we have in this collection, `gone` how many the
    /// enumeration did not mention.
    pub fn check(&self, held: usize, gone: usize) -> PruneVerdict {
        if gone <= self.always_allow_up_to || held == 0 {
            return PruneVerdict::Proceed;
        }
        let fraction = gone as f64 / held as f64;
        if fraction <= self.max_fraction {
            return PruneVerdict::Proceed;
        }
        PruneVerdict::Refuse {
            reason: format!(
                "the listing accounted for only {kept} of {held} stored row(s), so \
                 pruning would delete {gone} ({pct:.0}%). Refusing: an enumeration \
                 that lost most of a collection at once is more often a narrowed \
                 enumeration — a page cap, a changed filter, a downgraded token — \
                 than a real deletion, and the rows are the copy the provider does \
                 not control. Re-run with --reset-and-redownload to rebuild from \
                 scratch if the loss is real.",
                kept = held - gone,
                held = held,
                gone = gone,
                pct = fraction * 100.0,
            ),
        }
    }
}

/// [`PruneLimit::check`] plus the logging, since every caller wants both.
/// Returns whether to go ahead.
pub fn approve(what: &str, held: usize, gone: usize, limit: PruneLimit) -> bool {
    match limit.check(held, gone) {
        PruneVerdict::Proceed => true,
        PruneVerdict::Refuse { reason } => {
            tracing::warn!(
                event = "prune_refused",
                collection = what,
                held,
                would_delete = gone,
                "{reason}",
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The small-collection escape hatch. Without it a user with four
    /// conversations who deletes three gets a refusal every run forever,
    /// and learns to ignore the warning.
    #[test]
    fn a_small_absolute_prune_is_always_allowed() {
        let l = PruneLimit::default();
        assert_eq!(l.check(4, 3), PruneVerdict::Proceed);
        assert_eq!(l.check(10, 10), PruneVerdict::Proceed);
    }

    /// The case this exists for: a listing that came back nearly empty
    /// against a store that is not.
    #[test]
    fn losing_most_of_a_large_collection_is_refused() {
        let l = PruneLimit::default();
        let PruneVerdict::Refuse { reason } = l.check(1000, 990) else {
            panic!("990 of 1000 must be refused");
        };
        // The message has to name both numbers: "prune refused" alone
        // leaves the reader unable to tell a bug from a real mass delete.
        assert!(reason.contains("1000"), "{reason}");
        assert!(reason.contains("990"), "{reason}");
    }

    /// Ordinary churn is not blocked — a tenth of a big mailbox going is
    /// well within what a real archive clear-out looks like.
    #[test]
    fn ordinary_churn_proceeds() {
        let l = PruneLimit::default();
        assert_eq!(l.check(1000, 100), PruneVerdict::Proceed);
        assert_eq!(l.check(1000, 500), PruneVerdict::Proceed);
        assert!(matches!(l.check(1000, 501), PruneVerdict::Refuse { .. }));
    }

    /// An empty store cannot lose anything, and must not divide by zero.
    #[test]
    fn an_empty_collection_proceeds() {
        assert_eq!(PruneLimit::default().check(0, 0), PruneVerdict::Proceed);
    }
}
