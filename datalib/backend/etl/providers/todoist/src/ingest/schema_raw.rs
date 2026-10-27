//! The raw store: one `(id, payload)` table per kind of Todoist object,
//! keyed by Todoist's own id, the payload the object as the API sent
//! it. The tables keep the names `/sync` uses (`items` for tasks,
//! `notes` for task comments), since that is the shape they hold.
//! Beside them, `sync_state` holds the one position the download keeps,
//! Todoist's own sync token, and `coverage` the completion dates the
//! walk of completed tasks has read.

use std::marker::PhantomData;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, Migration, VolatilePath};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// One kind of object: its table, the key of the `/sync` answer it
/// comes under (`None` for one the sync does not send), what it gives up
/// to the sidecar (`<table>_bookkeeping.volatile_payload`), and the
/// fields never stored at all.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub table: &'static str,
    pub sync_key: Option<&'static str>,
    pub volatile: &'static [VolatilePath<'static>],
    pub secret: &'static [&'static str],
}

impl Kind {
    const fn synced(table: &'static str) -> Self {
        Self {
            table,
            sync_key: Some(table),
            volatile: &[],
            secret: &[],
        }
    }
}

/// A task's place in the Today and Upcoming views moves as other tasks
/// come and go there, with the task itself untouched.
const DAY_ORDER: VolatilePath<'static> = &["day_order"];

pub const PROJECTS: Kind = Kind::synced("projects");
pub const SECTIONS: Kind = Kind::synced("sections");
pub const ITEMS: Kind = Kind {
    volatile: &[DAY_ORDER],
    ..Kind::synced("items")
};
pub const NOTES: Kind = Kind::synced("notes");
pub const PROJECT_NOTES: Kind = Kind::synced("project_notes");
pub const LABELS: Kind = Kind::synced("labels");
pub const COLLABORATORS: Kind = Kind::synced("collaborators");

/// The account. Its karma and counts move as tasks are done, and say
/// nothing about the account; its API token, and the websocket URL that
/// carries it, are a credential and never stored.
pub const USER: Kind = Kind {
    table: "user",
    sync_key: Some("user"),
    volatile: &[
        &["karma"],
        &["karma_trend"],
        &["completed_count"],
        &["completed_today"],
        &["status"],
        &["features"],
    ],
    secret: &["token", "websocket_url", "urgent_reminder_device"],
};

/// Tasks as `/tasks/completed/by_completion_date` answers them, which
/// is a shape of its own (`completed_by_uid`, `note_count`), so they are
/// kept apart from the `items` the sync sends.
pub const COMPLETED_ITEMS: Kind = Kind {
    table: "completed_items",
    sync_key: None,
    volatile: &[DAY_ORDER],
    secret: &[],
};

/// The resource types the download asks `/sync` for, as the kinds they
/// fill.
pub const SYNCED: &[Kind] = &[
    USER,
    PROJECTS,
    SECTIONS,
    ITEMS,
    NOTES,
    PROJECT_NOTES,
    LABELS,
    COLLABORATORS,
];

pub const KINDS: &[Kind] = &[
    USER,
    PROJECTS,
    SECTIONS,
    ITEMS,
    NOTES,
    PROJECT_NOTES,
    LABELS,
    COLLABORATORS,
    COMPLETED_ITEMS,
];

/// The `sync_state` row: the token the next `/sync` sends, written in
/// the transaction that stores what the answer carrying it held.
pub const SYNC_STATE: &str = "sync_state";
pub const SYNC_STATE_ROW: &str = "sync";

pub fn sync_state_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SYNC_STATE} (\
         id TEXT PRIMARY KEY, sync_token TEXT NOT NULL, full_sync_date_utc TEXT NULL)"
    )
}

/// The `coverage` scope of the walk of completed tasks: spans of
/// completion time, as `YYYY-MM-DDTHH:MM:SSZ`.
pub const COMPLETED_SCOPE: &str = "completed_items";

/// One row of a `Kind`'s table, written through the shared bulk upsert.
/// `T` names the table at the type level, which the upsert needs.
pub struct Row<T> {
    pub id: String,
    pub payload: String,
    table: PhantomData<fn() -> T>,
}

impl<T> Row<T> {
    pub fn new(id: String, payload: String) -> Self {
        Self {
            id,
            payload,
            table: PhantomData,
        }
    }
}

pub trait Table {
    const KIND: Kind;
}

impl<T: Table> BulkUpsertable for Row<T> {
    const TABLE: &'static str = T::KIND.table;
    const TYPED_COLUMNS: &'static [&'static str] = &[];

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id).bind(&self.payload)
    }
}

macro_rules! tables {
    ($($marker:ident => $kind:ident),* $(,)?) => {
        $(
            pub enum $marker {}
            impl Table for $marker {
                const KIND: Kind = $kind;
            }
        )*
    };
}

tables! {
    UserT => USER,
    ProjectsT => PROJECTS,
    SectionsT => SECTIONS,
    ItemsT => ITEMS,
    NotesT => NOTES,
    ProjectNotesT => PROJECT_NOTES,
    LabelsT => LABELS,
    CollaboratorsT => COLLABORATORS,
    CompletedItemsT => COMPLETED_ITEMS,
}

pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = KINDS
        .iter()
        .map(|k| dr::wire_payload_table_ddl(k.table, &[]))
        .collect();
    out.push(sync_state_ddl());
    out.push(datalib_etl_web::coverage::DDL.to_string());
    out.extend(KINDS.iter().map(|k| dr::bookkeeping_ddl_for(k.table)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_table_of_its_own() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.table).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len());
        assert!(SYNCED.iter().all(|k| k.sync_key.is_some()));
        assert_eq!(COMPLETED_ITEMS.sync_key, None);
        assert_eq!(<Row<ItemsT> as BulkUpsertable>::TABLE, "items");
    }
}
