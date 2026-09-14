//! What a render reads from the raw store, and how a run says so.
//!
//! [`RawRange`] is the driver's view of the raw store for one run — the
//! commit to diff from, the commit to read at, and the buckets it already
//! knows are stale — handed to a provider's parse. [`Inputs`] collects the
//! rows one bucket asked for while it is built, and [`Lookup`] is a map
//! that records every key asked of it, found or not, so a lookup table
//! (users, channels, recipients) cannot be read without being declared.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use datalib_etl::pin::{self, Pin};
use sqlx::SqlitePool;

pub use crate::indexed_markdown::Input;

/// The raw store as one render run sees it.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawRange<'a> {
    /// The commit the previous run consumed; `None` renders everything.
    pub cursor: Option<&'a str>,
    /// The commit the driver pinned, when it did. Read at this commit,
    /// never HEAD, so the rows loaded are the rows `stale` was computed
    /// against.
    pub pin: Option<&'a str>,
    /// Buckets whose declared inputs moved since `cursor`, from the
    /// driver's reverse lookup. `None` when the driver could not say.
    pub stale: Option<&'a HashSet<String>>,
}

impl<'a> RawRange<'a> {
    /// No cursor and nothing pinned: read HEAD and render everything.
    pub fn cold() -> Self {
        Self::default()
    }

    /// The provider's own diff from `cursor`, with nothing declared yet.
    pub fn from_cursor(cursor: Option<&'a str>) -> Self {
        Self {
            cursor,
            ..Self::default()
        }
    }

    /// The driver's pin, else HEAD; `None` when nothing is committed.
    pub async fn pin(&self, pool: &SqlitePool) -> Result<Option<Pin>> {
        match self.pin {
            Some(commit) => Pin::at(commit).map(Some),
            None => pin::head(pool).await,
        }
    }

    /// What to render: the driver's stale set joined with the provider's
    /// forward scan, or everything when either side could not narrow.
    pub fn narrow(&self, forward: Option<&HashSet<String>>) -> Option<HashSet<String>> {
        match (self.stale, forward) {
            (Some(stale), Some(forward)) => Some(stale.union(forward).cloned().collect()),
            _ => None,
        }
    }
}

/// The rows one bucket asked for, in the order the store diffs them.
/// Interior-mutable so a lookup through a shared `&` still records.
#[derive(Debug, Default)]
pub struct Inputs {
    seen: Mutex<BTreeSet<(String, String)>>,
}

impl Clone for Inputs {
    fn clone(&self) -> Self {
        Self {
            seen: Mutex::new(self.seen.lock().unwrap().clone()),
        }
    }
}

impl Inputs {
    pub fn read(&self, table: &str, id: &str) {
        self.seen
            .lock()
            .unwrap()
            .insert((table.to_string(), id.to_string()));
    }

    pub fn read_all<'i>(&self, table: &str, ids: impl IntoIterator<Item = &'i str>) {
        for id in ids {
            self.read(table, id);
        }
    }

    /// A map over `table`'s rows that records every key asked of it.
    pub fn lookup<'m, V>(
        &'m self,
        table: &'static str,
        map: &'m BTreeMap<String, V>,
    ) -> Lookup<'m, V> {
        Lookup {
            table,
            map,
            inputs: self,
        }
    }

    pub fn declared(&self) -> Vec<Input> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(table, id)| Input::new(table, id))
            .collect()
    }
}

/// A read-only view of one raw table's rows, keyed by primary key, that
/// declares every key it is asked for — a miss too, since the row's
/// arrival is a change the bucket must see.
pub struct Lookup<'a, V> {
    table: &'static str,
    map: &'a BTreeMap<String, V>,
    inputs: &'a Inputs,
}

impl<V> Clone for Lookup<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> Copy for Lookup<'_, V> {}

impl<'a, V> Lookup<'a, V> {
    pub fn get(&self, id: &str) -> Option<&'a V> {
        self.inputs.read(self.table, id);
        self.map.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lookup_declares_hits_and_misses_alike() {
        let inputs = Inputs::default();
        let users: BTreeMap<String, String> = [("u1".to_string(), "Anne".to_string())].into();
        let lookup = inputs.lookup("users", &users);
        assert_eq!(lookup.get("u1").map(String::as_str), Some("Anne"));
        assert_eq!(lookup.get("u2"), None);
        inputs.read("messages", "m1");
        assert_eq!(
            inputs.declared(),
            vec![
                Input::new("messages", "m1"),
                Input::new("users", "u1"),
                Input::new("users", "u2"),
            ]
        );
    }

    #[test]
    fn narrowing_needs_both_sides() {
        let stale: HashSet<String> = ["a".to_string()].into();
        let forward: HashSet<String> = ["b".to_string()].into();
        let range = RawRange {
            cursor: Some("c1"),
            pin: Some("c2"),
            stale: Some(&stale),
        };
        let both = range.narrow(Some(&forward)).unwrap();
        assert_eq!(both.len(), 2);
        assert!(
            range.narrow(None).is_none(),
            "a fan-out hit renders everything"
        );
        assert!(
            RawRange::from_cursor(Some("c1"))
                .narrow(Some(&forward))
                .is_none(),
            "nothing declared yet: the reverse lookup cannot vouch for the rest"
        );
    }
}
