//! Doltlite-backed raw store for the `calendar` provider.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable};
use sqlx::Row;

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, AccountRow, CalendarRow, GoogleEventRow, IcsObjectRow};

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

/// The account row, for render.
#[derive(Debug, Clone, Default)]
pub struct LoadedAccount {
    pub id: String,
    pub method: String,
    pub server_url: Option<String>,
    pub login: Option<String>,
}

/// A calendar, for render.
#[derive(Debug, Clone)]
pub struct LoadedCalendar {
    pub id: String,
    pub display_name: Option<String>,
    pub time_zone: Option<String>,
}

/// One `ics_objects` row, for render.
#[derive(Debug, Clone)]
pub struct LoadedIcsObject {
    pub id: String,
    pub calendar_id: String,
    pub uid: String,
    pub ics: String,
}

/// One `google_events` row, for render.
#[derive(Debug, Clone)]
pub struct LoadedGoogleEvent {
    pub id: String,
    pub calendar_id: String,
    pub event: serde_json::Value,
}

impl RawDb {
    async fn upsert<T: BulkUpsertable>(&self, rows: &[T], what: &str) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self
            .pool()
            .begin()
            .await
            .with_context(|| format!("begin {what} tx"))?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit()
            .await
            .with_context(|| format!("commit {what} tx"))?;
        Ok(())
    }

    // ── accounts and calendars ──────────────────────────────────────

    pub async fn upsert_account(&self, row: &AccountRow) -> Result<()> {
        self.upsert(std::slice::from_ref(row), "account").await
    }

    pub async fn upsert_calendars(&self, rows: &[CalendarRow]) -> Result<()> {
        self.upsert(rows, "calendars").await
    }

    /// The position the last completed sync of this calendar left; `None`
    /// when it has never finished one, so the next is a full listing.
    pub async fn sync_token(&self, calendar_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT sync_token FROM calendars WHERE id = ?")
            .bind(calendar_id)
            .fetch_optional(self.pool())
            .await
            .context("select sync_token")?;
        Ok(row
            .and_then(|r| r.try_get::<Option<String>, _>("sync_token").ok().flatten())
            .filter(|t| !t.is_empty()))
    }

    /// Called only once every change the token covers is stored, so an
    /// interrupted sync resumes from the previous one.
    pub async fn set_sync_token(&self, calendar_id: &str, token: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE calendars SET sync_token = ? WHERE id = ?")
            .bind(token)
            .bind(calendar_id)
            .execute(self.pool())
            .await
            .context("update sync_token")?;
        Ok(())
    }

    // ── ics_objects ─────────────────────────────────────────────────

    pub async fn upsert_ics_objects(&self, rows: &[IcsObjectRow]) -> Result<()> {
        self.upsert(rows, "ics_objects").await
    }

    /// Every stored object of a calendar, by CalDAV href, with its uid.
    pub async fn ics_hrefs(&self, calendar_id: &str) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT href, uid FROM ics_objects WHERE calendar_id = ? AND href IS NOT NULL",
        )
        .bind(calendar_id)
        .fetch_all(self.pool())
        .await
        .context("select ics hrefs")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.try_get("href").ok()?, r.try_get("uid").ok()?)))
            .collect())
    }

    pub async fn ics_uids(&self, calendar_id: &str) -> Result<HashSet<String>> {
        let uids: Vec<String> =
            sqlx::query_scalar("SELECT uid FROM ics_objects WHERE calendar_id = ?")
                .bind(calendar_id)
                .fetch_all(self.pool())
                .await
                .context("select ics uids")?;
        Ok(uids.into_iter().collect())
    }

    /// Drop the objects of one calendar with these uids, and their
    /// sidecar rows. Idempotent.
    pub async fn delete_ics_uids(&self, calendar_id: &str, uids: &[String]) -> Result<()> {
        let ids: Vec<String> = uids
            .iter()
            .map(|u| super::schema_raw::event_pk(calendar_id, u))
            .collect();
        self.delete_ids("ics_objects", &ids).await
    }

    // ── google_events ───────────────────────────────────────────────

    pub async fn upsert_google_events(&self, rows: &[GoogleEventRow]) -> Result<()> {
        self.upsert(rows, "google_events").await
    }

    pub async fn google_event_ids(&self, calendar_id: &str) -> Result<HashSet<String>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT event_id FROM google_events WHERE calendar_id = ?")
                .bind(calendar_id)
                .fetch_all(self.pool())
                .await
                .context("select google event ids")?;
        Ok(ids.into_iter().collect())
    }

    /// The stored occurrences of these series.
    pub async fn google_occurrences_of(
        &self,
        calendar_id: &str,
        series_ids: &[String],
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for series in series_ids {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT event_id FROM google_events
                 WHERE calendar_id = ? AND recurring_event_id = ?",
            )
            .bind(calendar_id)
            .bind(series)
            .fetch_all(self.pool())
            .await
            .context("select google occurrences")?;
            out.extend(ids);
        }
        Ok(out)
    }

    pub async fn delete_google_events(
        &self,
        calendar_id: &str,
        event_ids: &[String],
    ) -> Result<()> {
        let ids: Vec<String> = event_ids
            .iter()
            .map(|e| super::schema_raw::event_pk(calendar_id, e))
            .collect();
        self.delete_ids("google_events", &ids).await
    }

    async fn delete_ids(&self, table: &'static str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin delete tx")?;
        for id in ids {
            // Audited: `table` is a `&'static str` at every callsite.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE id = ?"
            )))
            .bind(id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("delete {table} {id}"))?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table}_bookkeeping WHERE id = ?"
            )))
            .bind(id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("delete {table}_bookkeeping {id}"))?;
        }
        tx.commit().await.context("commit delete tx")?;
        Ok(())
    }

    // ── render reads ────────────────────────────────────────────────

    pub async fn load_account(&self) -> Result<Option<LoadedAccount>> {
        // Audited: the interpolation is a table name this handle chose.
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, method, server_url, login FROM {} ORDER BY id LIMIT 1",
            self.reads().table("accounts")
        )))
        .fetch_optional(self.pool())
        .await
        .context("select account")?;
        Ok(row.map(|r| LoadedAccount {
            id: r.try_get("id").unwrap_or_default(),
            method: r.try_get("method").unwrap_or_default(),
            server_url: r.try_get::<Option<String>, _>("server_url").ok().flatten(),
            login: r.try_get::<Option<String>, _>("login").ok().flatten(),
        }))
    }

    pub async fn load_calendars(&self) -> Result<Vec<LoadedCalendar>> {
        // Audited: the interpolation is a table name this handle chose.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, display_name, time_zone FROM {} ORDER BY id",
            self.reads().table("calendars")
        )))
        .fetch_all(self.pool())
        .await
        .context("select calendars")?;
        Ok(rows
            .into_iter()
            .map(|r| LoadedCalendar {
                id: r.try_get("id").unwrap_or_default(),
                display_name: r
                    .try_get::<Option<String>, _>("display_name")
                    .ok()
                    .flatten(),
                time_zone: r.try_get::<Option<String>, _>("time_zone").ok().flatten(),
            })
            .collect())
    }

    pub async fn load_ics_objects(&self) -> Result<Vec<LoadedIcsObject>> {
        // Audited: the interpolation is a table name this handle chose.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, calendar_id, uid, json_extract(payload, '$.ics') AS ics
             FROM {} ORDER BY id",
            self.reads().table("ics_objects")
        )))
        .fetch_all(self.pool())
        .await
        .context("select ics_objects")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(LoadedIcsObject {
                    id: r.try_get("id").ok()?,
                    calendar_id: r.try_get("calendar_id").ok()?,
                    uid: r.try_get("uid").ok()?,
                    ics: r.try_get::<Option<String>, _>("ics").ok().flatten()?,
                })
            })
            .collect())
    }

    pub async fn load_google_events(&self) -> Result<Vec<LoadedGoogleEvent>> {
        // Audited: the interpolation is a table name this handle chose.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, calendar_id, json(payload) AS payload FROM {} ORDER BY id",
            self.reads().table("google_events")
        )))
        .fetch_all(self.pool())
        .await
        .context("select google_events")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let payload: Option<String> = r.try_get("payload").ok().flatten();
            let Some(event) = payload.and_then(|p| serde_json::from_str(&p).ok()) else {
                tracing::warn!(event = "calendar_google_payload_unreadable", id = %id, "a stored Google event is not JSON; skipped it");
                continue;
            };
            out.push(LoadedGoogleEvent {
                id,
                calendar_id: r.try_get("calendar_id").unwrap_or_default(),
                event,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::{event_pk, DATA_TABLES};

    #[tokio::test]
    async fn open_creates_every_table_and_its_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("entities.doltlite_db"))
            .await
            .unwrap();
        for t in DATA_TABLES {
            for name in [t.to_string(), format!("{t}_bookkeeping")] {
                let n: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?",
                )
                .bind(&name)
                .fetch_one(db.pool())
                .await
                .unwrap();
                assert_eq!(n, 1, "{name}");
            }
        }
        db.close().await;
    }

    #[tokio::test]
    async fn a_sync_token_survives_a_calendar_upsert_and_deletes_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("entities.doltlite_db"))
            .await
            .unwrap();
        let cal = CalendarRow {
            id: "bridge".into(),
            account_id: "caldav.enterprise.test".into(),
            display_name: Some("Bridge".into()),
            ..Default::default()
        };
        db.upsert_calendars(std::slice::from_ref(&cal))
            .await
            .unwrap();
        db.set_sync_token("bridge", Some("tok-1")).await.unwrap();
        db.upsert_calendars(&[cal]).await.unwrap();
        assert_eq!(
            db.sync_token("bridge").await.unwrap().as_deref(),
            Some("tok-1")
        );

        let row = IcsObjectRow::new(
            "bridge",
            "staff",
            Some("/c/staff.ics".into()),
            None,
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
        );
        db.upsert_ics_objects(&[row]).await.unwrap();
        assert_eq!(
            db.ics_hrefs("bridge")
                .await
                .unwrap()
                .get("/c/staff.ics")
                .map(String::as_str),
            Some("staff")
        );
        db.delete_ics_uids("bridge", &["staff".into()])
            .await
            .unwrap();
        db.delete_ics_uids("bridge", &["staff".into()])
            .await
            .unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ics_objects_bookkeeping WHERE id = ?")
                .bind(event_pk("bridge", "staff"))
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(n, 0);
        db.close().await;
    }
}
