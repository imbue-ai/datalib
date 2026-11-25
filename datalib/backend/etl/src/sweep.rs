//! Sweep markers: when a listing was last read whole, so that a source
//! synced often does not list it again each time. A marker only schedules;
//! it is never progress, and what is owed never depends on one.

use anyhow::{Context, Result};
use datalib_time::IsoOffsetTimestamp;
use sqlx::SqliteExecutor;

/// One source type's markers, kept in `sync_scope_state` as
/// `<source>:sweep:<key>`.
#[derive(Debug, Clone, Copy)]
pub struct Sweeps {
    source: &'static str,
}

impl Sweeps {
    pub const fn new(source: &'static str) -> Self {
        Self { source }
    }

    pub fn scope(&self, key: &str) -> String {
        format!("{}:sweep:{key}", self.source)
    }

    /// How long before `now` `key` was last swept. `now` is the run's
    /// own: the answer decides whether a listing is requested at all.
    pub async fn age<'e>(
        &self,
        db: impl SqliteExecutor<'e>,
        key: &str,
        now: &IsoOffsetTimestamp,
    ) -> Result<Option<chrono::Duration>> {
        let stamp: Option<String> =
            sqlx::query_scalar("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
                .bind(self.scope(key))
                .fetch_optional(db)
                .await
                .with_context(|| format!("read the sweep marker {key}"))?;
        stamp.map(|s| age_of(&s, now)).transpose()
    }

    /// Record that `key` was swept at `now`, the run's own. Call it in the
    /// transaction that stores the end of the sweep.
    pub async fn record<'e>(
        &self,
        db: impl SqliteExecutor<'e>,
        key: &str,
        now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        crate::doltlite_raw::upsert_scope_state(db, &self.scope(key), &now.to_rfc3339()).await
    }

    pub async fn forget<'e>(&self, db: impl SqliteExecutor<'e>, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM sync_scope_state WHERE scope = ?")
            .bind(self.scope(key))
            .execute(db)
            .await
            .with_context(|| format!("forget the sweep marker {key}"))?;
        Ok(())
    }
}

/// How long before `now` a marker's `stamp` is.
pub fn age_of(stamp: &str, now: &IsoOffsetTimestamp) -> Result<chrono::Duration> {
    let at = datalib_time::parse_strict(stamp)
        .with_context(|| format!("parse the sweep marker {stamp:?}"))?;
    Ok(now.inner() - at.inner())
}
