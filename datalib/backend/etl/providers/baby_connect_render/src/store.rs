//! What a render reads from the `baby_connect` raw store at its pin:
//! every table, whole. A family's years of entries are tens of
//! thousands of small rows; what is narrowed is which children render.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl_baby_connect::ingest::schema_raw::{ENTRIES, KIDS, USERS};
use datalib_etl_baby_connect::ingest::RawDb;
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;
use sqlx::Row as _;

pub type Rows = BTreeMap<String, Value>;

/// The raw store at the render's pin, by table and id.
#[derive(Debug, Default)]
pub struct Store {
    pub kids: Rows,
    pub users: Rows,
    pub entries: Rows,
    /// The children to render, by id; `None` renders every one.
    pub render: Option<HashSet<String>>,
}

pub fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// An id Baby Connect sends as a number, or as a string.
pub fn id_of(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

impl Store {
    /// The render unit a changed row of `table` belongs to, at the pin: the
    /// child an entry is for. A changed child or person is named on every
    /// child's document, so names none here and widens the render in
    /// [`load`].
    pub fn render_unit_of(&self, table: &str, id: &str) -> Option<String> {
        if table != ENTRIES.table {
            return None;
        }
        let e = self.entries.get(id)?;
        id_of(e, "Kid")
    }

    pub fn renders(&self, render_unit: &str) -> bool {
        self.render.as_ref().is_none_or(|r| r.contains(render_unit))
    }

    /// Every child with an entry, in order.
    pub fn render_units(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .values()
            .filter_map(|e| id_of(e, "Kid"))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        out.sort();
        out
    }
}

async fn read(db: &RawDb, table: &str) -> Result<Rows> {
    // Audited: `table` is one of this crate's `&'static str` table names.
    let sql = format!("SELECT id, json(payload) AS payload FROM \"{table}\"");
    let mut out = Rows::new();
    for row in sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(db.pool())
        .await
        .with_context(|| format!("read {table}"))?
    {
        let id: String = row.try_get("id")?;
        let Some(payload) = row.try_get::<Option<String>, _>("payload")? else {
            continue;
        };
        let value: Value = serde_json::from_str(&payload)
            .with_context(|| format!("{table} row {id} is not JSON"))?;
        out.insert(id, value);
    }
    Ok(out)
}

pub async fn load(db: &RawDb, range: RawRange<'_>) -> Result<Store> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let mut store = Store {
        kids: read(db, KIDS.table).await?,
        users: read(db, USERS.table).await?,
        entries: read(db, ENTRIES.table).await?,
        render: None,
    };
    let tables = [KIDS.table, USERS.table, ENTRIES.table];
    let changed = changed_rows(db.pool(), range, pin, &tables).await?;
    // A renamed person is named on every child's document, so a change to
    // a child or a person renders them all.
    let forward = changed.and_then(|by_table| {
        let touches_all = by_table
            .iter()
            .any(|(table, ids)| *table != ENTRIES.table && !ids.is_empty());
        (!touches_all).then(|| {
            by_table
                .iter()
                .flat_map(|(table, ids)| ids.iter().map(move |id| (table, id)))
                .filter_map(|(table, id)| store.render_unit_of(table, id))
                .collect::<HashSet<String>>()
        })
    });
    store.render = range.narrow(forward.as_ref());
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_entry_belongs_to_its_child() {
        let mut s = Store::default();
        s.entries.insert(
            "1".into(),
            json!({"Kid": 5629499534213121_u64, "Pdt": 681012}),
        );
        s.entries.insert(
            "2".into(),
            json!({"Kid": "5629499534213121", "Pdt": 690101}),
        );
        assert_eq!(
            s.render_unit_of("entries", "1").as_deref(),
            Some("5629499534213121")
        );
        assert_eq!(s.render_unit_of("kids", "5629499534213121"), None);
        assert_eq!(s.render_units(), vec!["5629499534213121".to_string()]);
    }
}
