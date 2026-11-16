//! The doltlite raw store, and the one way a record is written into it:
//! keyed by its id, its volatile fields split into the sidecar, stamped
//! the first time it is seen.

use anyhow::{Context, Result};
use datalib_etl::bulk::bulk_upsert_first_seen_in_tx;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::schema_raw::*;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl(), LADDER);

/// Baby Connect's ids are numbers too large for a double, sent as JSON
/// numbers; serde_json keeps their digits. A string id is taken as it is.
pub fn id_of(record: &Value) -> Option<String> {
    match record.get("Id")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Writes `records` into `kind`'s table in `tx`. A record without an id
/// is not one, and is skipped; the count is of the ones written. The
/// sidecar is stamped the first time an id is seen only, so a day read
/// again with nothing changed commits nothing.
pub async fn write(
    tx: &mut Transaction<'_, Sqlite>,
    kind: Kind,
    records: &[Value],
    now: &IsoOffsetTimestamp,
) -> Result<usize> {
    match kind.table {
        t if t == KIDS.table => write_as::<KidsT>(tx, records, now).await,
        t if t == USERS.table => write_as::<UsersT>(tx, records, now).await,
        t if t == ENTRIES.table => write_as::<EntriesT>(tx, records, now).await,
        other => anyhow::bail!("no table {other}"),
    }
}

async fn write_as<T: Table>(
    tx: &mut Transaction<'_, Sqlite>,
    records: &[Value],
    now: &IsoOffsetTimestamp,
) -> Result<usize> {
    let kind = T::KIND;
    let mut rows: Vec<Row<T>> = Vec::with_capacity(records.len());
    let mut volatile: Vec<(String, Value)> = Vec::new();
    for record in records {
        let Some(id) = id_of(record) else {
            continue;
        };
        let (base, split) = dr::split_volatile(record, kind.volatile);
        let payload = serde_json::to_string(&base)
            .with_context(|| format!("serialize {}={id}", kind.table))?;
        if let Some(split) = split {
            volatile.push((id.clone(), split));
        }
        rows.push(Row::new(id, payload));
    }
    bulk_upsert_first_seen_in_tx(tx, &rows, now).await?;
    let pairs: Vec<(&str, &Value)> = volatile.iter().map(|(id, v)| (id.as_str(), v)).collect();
    dr::set_volatile_payloads_in_tx(tx, kind.table, &pairs).await?;
    Ok(rows.len())
}

/// The kids the store holds, as the home page last listed them.
pub async fn kids(tx: &mut Transaction<'_, Sqlite>) -> Result<Vec<Value>> {
    let payloads: Vec<Option<String>> =
        sqlx::query_scalar("SELECT json(payload) FROM kids ORDER BY id")
            .fetch_all(&mut **tx)
            .await
            .context("read the kids")?;
    Ok(payloads
        .into_iter()
        .flatten()
        .filter_map(|p| serde_json::from_str(&p).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An id past 2^53 is a JSON number Baby Connect sends whole; read
    /// as a double it would land on its neighbour.
    #[test]
    fn a_large_numeric_id_keeps_every_digit() {
        let v: Value = serde_json::from_str(r#"{"Id": 5629499534213121}"#).unwrap();
        assert_eq!(id_of(&v).as_deref(), Some("5629499534213121"));
        assert_eq!(id_of(&json!({"Id": "abc"})).as_deref(), Some("abc"));
        assert_eq!(id_of(&json!({"Id": ""})), None);
        assert_eq!(id_of(&json!({})), None);
    }
}
