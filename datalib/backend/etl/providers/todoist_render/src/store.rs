//! What a render reads from the `todoist` raw store at its pin: every
//! table, whole. An account's tasks and comments are small enough that
//! reading them whole costs less than working out which ones to read;
//! what is narrowed is which projects render.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use datalib_etl_todoist::ingest::schema_raw::{
    COLLABORATORS, COMPLETED_ITEMS, ITEMS, NOTES, PROJECTS, PROJECT_NOTES, SECTIONS, USER,
};
use datalib_etl_todoist::ingest::RawDb;
use serde_json::Value;
use sqlx::Row as _;

/// Every table a project's document reads but the account's own.
const TABLES: [&str; 7] = [
    PROJECTS.table,
    SECTIONS.table,
    ITEMS.table,
    COMPLETED_ITEMS.table,
    NOTES.table,
    PROJECT_NOTES.table,
    COLLABORATORS.table,
];

pub type Rows = BTreeMap<String, Value>;

/// The raw store at the render's pin, by table and id.
#[derive(Debug, Default)]
pub struct Store {
    pub user: Option<(String, Value)>,
    pub projects: Rows,
    pub sections: Rows,
    pub items: Rows,
    pub completed: Rows,
    pub notes: Rows,
    pub project_notes: Rows,
    pub collaborators: Rows,
    /// The projects to render; `None` renders every one.
    pub render: Option<HashSet<String>>,
}

pub fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A user id: a string in API v1, though the reference shows a
/// project comment's `posted_uid` as a number.
pub fn uid_of(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool) == Some(true)
}

impl Store {
    /// The project a task row is in.
    fn project_of_task(&self, task: &str) -> Option<&str> {
        self.items
            .get(task)
            .or_else(|| self.completed.get(task))
            .and_then(|t| str_of(t, "project_id"))
    }

    /// The project a changed row of `table` belongs to, at the pin.
    pub fn project_of(&self, table: &str, id: &str) -> Option<String> {
        let row = |rows: &Rows| rows.get(id).cloned();
        match table {
            t if t == PROJECTS.table => Some(id.to_string()),
            t if t == SECTIONS.table => row(&self.sections)
                .as_ref()
                .and_then(|s| str_of(s, "project_id").map(String::from)),
            t if t == ITEMS.table || t == COMPLETED_ITEMS.table => {
                self.project_of_task(id).map(String::from)
            }
            t if t == NOTES.table => row(&self.notes)
                .as_ref()
                .and_then(|n| str_of(n, "item_id"))
                .and_then(|task| self.project_of_task(task))
                .map(String::from),
            t if t == PROJECT_NOTES.table => row(&self.project_notes)
                .as_ref()
                .and_then(|n| str_of(n, "project_id").map(String::from)),
            _ => None,
        }
    }

    pub fn renders(&self, project: &str) -> bool {
        self.render.as_ref().is_none_or(|r| r.contains(project))
    }

    /// Every project a document is owed for: the ones the store holds,
    /// and any a task names that it does not.
    pub fn project_ids(&self) -> Vec<String> {
        let mut ids: HashSet<String> = self.projects.keys().cloned().collect();
        for t in self.items.values().chain(self.completed.values()) {
            if let Some(p) = str_of(t, "project_id") {
                ids.insert(p.to_string());
            }
        }
        let mut ids: Vec<String> = ids.into_iter().collect();
        ids.sort();
        ids
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
        user: read(db, USER.table).await?.into_iter().next(),
        projects: read(db, PROJECTS.table).await?,
        sections: read(db, SECTIONS.table).await?,
        items: read(db, ITEMS.table).await?,
        completed: read(db, COMPLETED_ITEMS.table).await?,
        notes: read(db, NOTES.table).await?,
        project_notes: read(db, PROJECT_NOTES.table).await?,
        collaborators: read(db, COLLABORATORS.table).await?,
        render: None,
    };
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    let forward = changed.map(|by_table| {
        by_table
            .iter()
            .flat_map(|(table, ids)| ids.iter().map(move |id| (table, id)))
            .filter_map(|(table, id)| store.project_of(table, id))
            .collect::<HashSet<String>>()
    });
    store.render = range.narrow(forward.as_ref());
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A comment reaches its project through its task, whether the task
    /// is open or only in the completed list.
    #[test]
    fn a_changed_row_names_its_project() {
        let mut s = Store::default();
        s.sections.insert("s1".into(), json!({"project_id": "p1"}));
        s.items.insert("t1".into(), json!({"project_id": "p1"}));
        s.completed.insert("t2".into(), json!({"project_id": "p2"}));
        s.notes.insert("n1".into(), json!({"item_id": "t2"}));
        s.project_notes
            .insert("pn1".into(), json!({"project_id": "p3"}));
        assert_eq!(s.project_of("projects", "p9").as_deref(), Some("p9"));
        assert_eq!(s.project_of("sections", "s1").as_deref(), Some("p1"));
        assert_eq!(s.project_of("items", "t1").as_deref(), Some("p1"));
        assert_eq!(s.project_of("completed_items", "t2").as_deref(), Some("p2"));
        assert_eq!(s.project_of("notes", "n1").as_deref(), Some("p2"));
        assert_eq!(s.project_of("project_notes", "pn1").as_deref(), Some("p3"));
        assert_eq!(s.project_of("collaborators", "1701"), None);
    }
}
