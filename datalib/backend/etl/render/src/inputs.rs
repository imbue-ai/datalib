//! What a render reads from the raw store, and how a run says so.
//!
//! [`RawRange`] is the driver's view of the raw store for one run — the
//! commit to diff from, the commit to read at, and the buckets it already
//! knows are stale — handed to a provider's parse. [`Inputs`] collects the
//! rows one bucket asked for while it is built, and [`Lookup`] is a map
//! that records every key asked of it, found or not, so a lookup table
//! (users, channels, recipients) cannot be read without being declared.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

    /// Whether one bucket has to render: the driver found it stale, or
    /// could not say. For a provider whose store is one document.
    pub fn is_stale(&self, key: &str) -> bool {
        self.stale.is_none_or(|stale| stale.contains(key))
    }

    /// What to render: the driver's stale set joined with the provider's
    /// forward scan, or everything when either side could not narrow.
    /// For a provider whose bucket key is the raw id.
    pub fn narrow(&self, forward: Option<&HashSet<String>>) -> Option<HashSet<String>> {
        self.narrow_by(forward, |key| Some(key.to_string())).render
    }

    /// The same, for a provider whose bucket key is minted from the raw
    /// id: `to_raw` maps a stale bucket key back to the row to load, and
    /// a key it cannot map — the row is gone — comes back in `gone`, for
    /// the processor to declare with nothing so its documents go.
    pub fn narrow_by(
        &self,
        forward: Option<&HashSet<String>>,
        to_raw: impl Fn(&str) -> Option<String>,
    ) -> Narrowed {
        let (Some(stale), Some(forward)) = (self.stale, forward) else {
            return Narrowed::default();
        };
        let mut render = forward.clone();
        let mut gone = Vec::new();
        for key in stale {
            match to_raw(key) {
                Some(raw) => {
                    render.insert(raw);
                }
                None => gone.push(key.clone()),
            }
        }
        gone.sort();
        Narrowed {
            render: Some(render),
            gone,
        }
    }
}

/// [`RawRange::narrow_by`]'s answer.
#[derive(Debug, Default)]
pub struct Narrowed {
    /// Raw ids to load and render; `None` renders everything.
    pub render: Option<HashSet<String>>,
    /// Bucket keys the driver found stale whose raw row no longer exists.
    pub gone: Vec<String>,
}

/// One bucket a run rendered: its key and the rows it read — what a
/// processor declares through `RenderCtx::declare_bucket`.
#[derive(Debug, Clone)]
pub struct Bucket {
    pub key: String,
    pub inputs: Vec<Input>,
}

/// Every bucket a render pass produced.
pub type Buckets = Vec<Bucket>;

/// The rows the diff names as changed since the cursor, per table, for
/// the tables that exist — the forward half of a provider whose bucket
/// keys are minted from rows it has to load anyway: map each changed id
/// through the loaded rows to its bucket. `None` when there is no cursor
/// to diff from, or the store cannot resolve it: everything renders.
pub async fn changed_rows(
    pool: &SqlitePool,
    range: RawRange<'_>,
    pin: &Pin,
    tables: &[&str],
) -> Result<Option<HashMap<String, HashSet<String>>>> {
    let Some(from) = range.cursor else {
        return Ok(None);
    };
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for table in tables {
        let exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(table)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            continue;
        }
        match datalib_etl::doltlite_raw::changed_keys(pool, table, from, pin.commit()).await {
            Ok(keys) => {
                out.insert(table.to_string(), keys.into_iter().collect());
            }
            Err(e) => {
                tracing::warn!(
                    table,
                    from,
                    error = %format!("{e:#}"),
                    "render: the cursor cannot be diffed; rendering everything"
                );
                return Ok(None);
            }
        }
    }
    Ok(Some(out))
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
    pub fn lookup<'m, M: Rows>(&'m self, table: &'static str, map: &'m M) -> Lookup<'m, M> {
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

/// One raw table's rows in memory, keyed by primary key.
pub trait Rows {
    type Row;
    fn row(&self, id: &str) -> Option<&Self::Row>;
}

impl<V> Rows for BTreeMap<String, V> {
    type Row = V;
    fn row(&self, id: &str) -> Option<&V> {
        self.get(id)
    }
}

impl<V> Rows for HashMap<String, V> {
    type Row = V;
    fn row(&self, id: &str) -> Option<&V> {
        self.get(id)
    }
}

/// A read-only view of one raw table's rows that declares every key it
/// is asked for — a miss too, since the row's arrival is a change the
/// bucket must see.
pub struct Lookup<'a, M> {
    table: &'static str,
    map: &'a M,
    inputs: &'a Inputs,
}

impl<M> Clone for Lookup<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Copy for Lookup<'_, M> {}

impl<'a, M: Rows> Lookup<'a, M> {
    pub fn get(&self, id: &str) -> Option<&'a M::Row> {
        self.inputs.read(self.table, id);
        self.map.row(id)
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
        let mapped = range.narrow_by(Some(&forward), |key| (key == "a").then(|| "raw-a".into()));
        assert_eq!(
            mapped.render.unwrap().len(),
            2,
            "a maps to its raw id, b is forward"
        );
        assert!(mapped.gone.is_empty());
        let unmapped = range.narrow_by(Some(&forward), |_| None);
        assert_eq!(
            unmapped.gone,
            vec!["a".to_string()],
            "an unmappable stale key is gone"
        );
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
