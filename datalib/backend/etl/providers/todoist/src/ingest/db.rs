//! The doltlite raw store, and the one way a record is written into it:
//! its secrets dropped, its label bag sorted, its volatile fields split
//! into the sidecar.

use anyhow::{Context, Result};
use datalib_etl::bulk::{bulk_upsert_first_seen_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{self as dr};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::schema_raw::*;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl(), LADDER);

/// How a write stamps the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    /// The record changed: stamp the fetch.
    Fetched,
    /// A record the answer carries whether or not it changed (the
    /// account): stamp the first time only, so an unchanged one commits
    /// nothing.
    FirstSeen,
}

/// `record` as it is stored: without the fields `kind` never keeps, and
/// with the label names a task carries, which Todoist promises in no
/// order, sorted.
pub fn canonical(kind: Kind, record: &Value) -> Value {
    let mut record = record.clone();
    if let Some(obj) = record.as_object_mut() {
        for key in kind.secret {
            obj.remove(*key);
        }
        if let Some(labels) = obj.get_mut("labels").and_then(Value::as_array_mut) {
            labels.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    record
}

/// Todoist's ids are strings in API v1; a number is read as its digits
/// all the same, so a stray numeric id is not a dropped record.
pub fn id_of(record: &Value) -> Option<String> {
    match record.get("id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The version a completed task is held at.
pub fn version_of(record: &Value) -> Option<String> {
    ["updated_at", "completed_at"]
        .iter()
        .find_map(|k| record.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

/// Writes `records` into `kind`'s table in `tx`. A record without an id
/// is not one, and is skipped; the count is of the ones written.
pub async fn write(
    tx: &mut Transaction<'_, Sqlite>,
    kind: Kind,
    records: &[Value],
    stamp: Stamp,
    now: &IsoOffsetTimestamp,
) -> Result<usize> {
    macro_rules! dispatch {
        ($($t:ident),*) => {
            match kind.table {
                $(t if t == $t::KIND.table => write_as::<$t>(tx, records, stamp, now).await,)*
                other => anyhow::bail!("no table {other}"),
            }
        };
    }
    dispatch!(
        UserT,
        ProjectsT,
        SectionsT,
        ItemsT,
        NotesT,
        ProjectNotesT,
        LabelsT,
        CollaboratorsT,
        CompletedItemsT
    )
}

async fn write_as<T: Table>(
    tx: &mut Transaction<'_, Sqlite>,
    records: &[Value],
    stamp: Stamp,
    now: &IsoOffsetTimestamp,
) -> Result<usize> {
    let kind = T::KIND;
    let mut rows: Vec<Row<T>> = Vec::with_capacity(records.len());
    let mut volatile: Vec<(String, Value)> = Vec::new();
    for record in records {
        let Some(id) = id_of(record) else {
            continue;
        };
        let (base, split) = dr::split_volatile(&canonical(kind, record), kind.volatile);
        let payload = serde_json::to_string(&base)
            .with_context(|| format!("serialize {}={id}", kind.table))?;
        if let Some(split) = split {
            volatile.push((id.clone(), split));
        }
        rows.push(Row::new(id, payload));
    }
    match stamp {
        Stamp::Fetched => bulk_upsert_in_tx(tx, &rows, now).await?,
        Stamp::FirstSeen => bulk_upsert_first_seen_in_tx(tx, &rows, now).await?,
    }
    let pairs: Vec<(&str, &Value)> = volatile.iter().map(|(id, v)| (id.as_str(), v)).collect();
    dr::set_volatile_payloads_in_tx(tx, kind.table, &pairs).await?;
    Ok(rows.len())
}

/// The token the next `/sync` sends: `*` (everything) when none is held.
pub async fn sync_token(tx: &mut Transaction<'_, Sqlite>) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT sync_token FROM sync_state WHERE id = ?")
        .bind(SYNC_STATE_ROW)
        .fetch_optional(&mut **tx)
        .await
        .context("read the sync token")
}

pub async fn set_sync_token(
    tx: &mut Transaction<'_, Sqlite>,
    token: &str,
    full_sync_date_utc: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sync_state (id, sync_token, full_sync_date_utc) VALUES (?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET sync_token = excluded.sync_token, \
         full_sync_date_utc = COALESCE(excluded.full_sync_date_utc, sync_state.full_sync_date_utc)",
    )
    .bind(SYNC_STATE_ROW)
    .bind(token)
    .bind(full_sync_date_utc)
    .execute(&mut **tx)
    .await
    .context("write the sync token")?;
    Ok(())
}

/// The account's `joined_at`, as the stored `user` row has it.
pub async fn joined_at(tx: &mut Transaction<'_, Sqlite>) -> Result<Option<String>> {
    let payloads: Vec<Option<String>> =
        sqlx::query_scalar("SELECT json(payload) FROM user ORDER BY id")
            .fetch_all(&mut **tx)
            .await
            .context("read the user row")?;
    Ok(payloads
        .into_iter()
        .flatten()
        .filter_map(|p| serde_json::from_str::<Value>(&p).ok())
        .find_map(|u| u.get("joined_at").and_then(Value::as_str).map(String::from)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The account's API token rides on the user object `/sync` sends;
    /// a raw store is no place for a credential.
    #[test]
    fn the_accounts_token_is_never_stored() {
        let user = json!({"id": "1701", "email": "picard@enterprise.test",
            "token": "0123456789abcdef", "websocket_url": "wss://ws.todoist.com/ws?token=abc",
            "urgent_reminder_device": {"device_token": "a1b2"}, "full_name": "Jean-Luc Picard"});
        let stored = canonical(USER, &user);
        assert_eq!(
            stored,
            json!({"id": "1701", "email": "picard@enterprise.test",
            "full_name": "Jean-Luc Picard"})
        );
    }

    #[test]
    fn a_tasks_labels_are_sorted() {
        let task = json!({"id": "t1", "labels": ["warp", "away-team"]});
        assert_eq!(
            canonical(ITEMS, &task)["labels"],
            json!(["away-team", "warp"])
        );
    }

    #[test]
    fn ids_are_strings_or_their_digits() {
        assert_eq!(id_of(&json!({"id": "6X7rM"})).as_deref(), Some("6X7rM"));
        assert_eq!(id_of(&json!({"id": 1701})).as_deref(), Some("1701"));
        assert_eq!(id_of(&json!({"id": ""})), None);
        assert_eq!(id_of(&json!({})), None);
    }
}
