//! The doltlite raw store, and the one way a record is written into it:
//! put in canonical order, its volatile fields split into the sidecar.

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
    /// A table read whole every run: stamp the first time only, so an
    /// unchanged record read again commits nothing.
    FirstSeen,
}

/// Sorts the id bags Linear returns in no promised order — `labelIds`
/// and every `{ nodes: [{ id }] }` connection at the top level — so an
/// unchanged record serializes the same way every time.
pub fn canonicalize(record: &mut Value) {
    let Some(obj) = record.as_object_mut() else {
        return;
    };
    for (key, value) in obj.iter_mut() {
        if key == "labelIds" {
            if let Some(ids) = value.as_array_mut() {
                ids.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
        } else if let Some(nodes) = value
            .as_object_mut()
            .filter(|c| c.len() == 1)
            .and_then(|c| c.get_mut("nodes"))
            .and_then(Value::as_array_mut)
        {
            nodes.sort_by(|a, b| {
                let id = |v: &Value| v.get("id").and_then(Value::as_str).map(String::from);
                id(a).cmp(&id(b))
            });
        }
    }
}

pub fn id_of(record: &Value) -> Option<&str> {
    record.get("id").and_then(Value::as_str)
}

pub fn updated_at(record: &Value) -> Option<&str> {
    record.get("updatedAt").and_then(Value::as_str)
}

/// Writes `records` into `kind`'s table in `tx`. A record without an id
/// is not one, and is skipped.
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
        TeamsT,
        UsersT,
        WorkflowStatesT,
        IssueLabelsT,
        ProjectStatusesT,
        CyclesT,
        ProjectsT,
        ProjectMilestonesT,
        InitiativesT,
        IssuesT,
        IssueHistoryT,
        CommentsT,
        DocumentsT,
        ProjectUpdatesT,
        AttachmentsT
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
        let mut record = record.clone();
        canonicalize(&mut record);
        let (base, split) = dr::split_volatile(&record, kind.volatile);
        let payload = serde_json::to_string(&base)
            .with_context(|| format!("serialize {}={id}", kind.table))?;
        rows.push(Row::new(id.to_string(), payload));
        if let Some(split) = split {
            volatile.push((id.to_string(), split));
        }
    }
    match stamp {
        Stamp::Fetched => bulk_upsert_in_tx(tx, &rows, now).await?,
        Stamp::FirstSeen => bulk_upsert_first_seen_in_tx(tx, &rows, now).await?,
    }
    let pairs: Vec<(&str, &Value)> = volatile.iter().map(|(id, v)| (id.as_str(), v)).collect();
    dr::set_volatile_payloads_in_tx(tx, kind.table, &pairs).await?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn id_bags_are_sorted_and_ordered_lists_left_alone() {
        let mut r = json!({
            "labelIds": ["l2", "l1"],
            "previousIdentifiers": ["ENG-9", "ENG-2"],
            "teams": {"nodes": [{"id": "t2"}, {"id": "t1"}]},
            "history": {"nodes": [{"id": "h2"}, {"id": "h1"}], "pageInfo": {}},
        });
        canonicalize(&mut r);
        assert_eq!(r["labelIds"], json!(["l1", "l2"]));
        assert_eq!(r["previousIdentifiers"], json!(["ENG-9", "ENG-2"]));
        assert_eq!(r["teams"]["nodes"], json!([{"id": "t1"}, {"id": "t2"}]));
        assert_eq!(
            r["history"]["nodes"],
            json!([{"id": "h2"}, {"id": "h1"}]),
            "only a bare connection is a bag"
        );
    }
}
