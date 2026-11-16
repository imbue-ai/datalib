//! The raw store: one `(id, payload)` table per kind of Baby Connect
//! object, keyed by Baby Connect's own id, the payload the object as the
//! web app was sent it. Beside them, `coverage` holds the days the walk
//! has read whole.

use std::marker::PhantomData;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, Migration, VolatilePath};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// One kind of object: its table, and what it gives up to the sidecar
/// (`<table>_bookkeeping.volatile_payload`).
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub table: &'static str,
    pub volatile: &'static [VolatilePath<'static>],
}

/// A picture's address is signed and expires, so it changes on every
/// page load with the picture untouched.
const SIGNED_PICTURE: VolatilePath<'static> = &["url_t"];

/// A child, as the home page's `myKids` lists them.
pub const KIDS: Kind = Kind {
    table: "kids",
    volatile: &[SIGNED_PICTURE],
};

/// Whoever logged an entry: a parent, a caregiver. A day's answer names
/// the ones its entries were logged by.
pub const USERS: Kind = Kind {
    table: "users",
    volatile: &[SIGNED_PICTURE],
};

/// One logged entry: a feed, a diaper, a sleep, a note, a photo. Who has
/// seen it (`readby`) grows as people open the app, with the entry
/// untouched.
pub const ENTRIES: Kind = Kind {
    table: "entries",
    volatile: &[&["readby"]],
};

pub const KINDS: &[Kind] = &[KIDS, USERS, ENTRIES];

/// The `coverage` scope of the walk: spans of days, as `YYYY-MM-DD`.
pub const DAYS_SCOPE: &str = "days";

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

pub enum KidsT {}
impl Table for KidsT {
    const KIND: Kind = KIDS;
}
pub enum UsersT {}
impl Table for UsersT {
    const KIND: Kind = USERS;
}
pub enum EntriesT {}
impl Table for EntriesT {
    const KIND: Kind = ENTRIES;
}

pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = KINDS
        .iter()
        .map(|k| dr::wire_payload_table_ddl(k.table, &[]))
        .collect();
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
        assert_eq!(<Row<EntriesT> as BulkUpsertable>::TABLE, "entries");
    }
}
