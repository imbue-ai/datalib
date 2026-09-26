//! The ordered results of recent searches. A search's rows are listed once
//! per query, sort and commit, then every page of it is a slice of the
//! list: the second page of a search does not run it again.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use datalib_unified_index::group::Within;
use datalib_unified_index::sort::Sort;

/// Searches kept. A list of every row of a large root is a few MB of
/// uuids; this many of those is the most it holds.
const CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub q: String,
    pub sort: Option<Sort>,
    /// The group whose rows these are; empty for the whole search.
    pub within: Vec<Within>,
    /// The commit the list was read at, `None` before the index has one.
    /// A search after the index moves is a different key, so a list never
    /// outlives the rows it names.
    pub at: Option<String>,
}

/// One row of a search's result, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub uuid: String,
    /// What qmd matched, for a free-text search: its score, and the words
    /// it matched on, shown as the row's Contents.
    pub hit: Option<(f64, String)>,
}

#[derive(Default)]
pub struct ResultCache(Mutex<VecDeque<(Key, Arc<Vec<Entry>>)>>);

impl ResultCache {
    pub fn get(&self, key: &Key) -> Option<Arc<Vec<Entry>>> {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let i = entries.iter().position(|(k, _)| k == key)?;
        let found = entries.remove(i)?;
        let list = found.1.clone();
        entries.push_front(found);
        Some(list)
    }

    pub fn put(&self, key: Key, list: Arc<Vec<Entry>>) {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(k, _)| k != &key);
        entries.push_front((key, list));
        entries.truncate(CAPACITY);
    }
}

/// The most rows one page carries.
pub const MAX_PAGE: usize = 100_000;

/// A page's size once it also reaches the row `through` names, wherever
/// that row is past `offset`: a grid re-reading the rows it holds, or
/// seeking a selected row, asks for them in one request.
pub fn reaching(list: &[Entry], offset: usize, limit: usize, through: Option<&str>) -> usize {
    let target = through.and_then(|uuid| list.iter().position(|e| e.uuid == uuid));
    let needed = target.map_or(0, |i| (i + 1).saturating_sub(offset));
    limit.max(needed).min(MAX_PAGE)
}

/// The `limit` entries from `offset`, and the offset of the next page, or
/// `None` when these reach the end.
pub fn page(list: &[Entry], offset: usize, limit: usize) -> (&[Entry], Option<usize>) {
    let start = offset.min(list.len());
    let end = start.saturating_add(limit).min(list.len());
    let next = (end < list.len()).then_some(end);
    (&list[start..end], next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                uuid: format!("u{i}"),
                hit: None,
            })
            .collect()
    }

    fn key(q: &str, at: &str) -> Key {
        Key {
            q: q.into(),
            sort: None,
            within: Vec::new(),
            at: Some(at.into()),
        }
    }

    #[test]
    fn pages_cover_the_list_once_and_say_where_the_next_starts() {
        let l = list(5);
        let (first, next) = page(&l, 0, 2);
        assert_eq!(first.len(), 2);
        assert_eq!(next, Some(2));
        let (last, next) = page(&l, 4, 2);
        assert_eq!(last[0].uuid, "u4");
        assert_eq!(next, None, "the last page says there is no more");
        let (exact, next) = page(&l, 3, 2);
        assert_eq!(exact.len(), 2);
        assert_eq!(next, None, "a page ending on the last row is the last");
    }

    #[test]
    fn a_page_stretches_to_reach_the_row_it_is_asked_through() {
        let l = list(10);
        assert_eq!(reaching(&l, 0, 2, Some("u6")), 7);
        assert_eq!(reaching(&l, 4, 2, Some("u6")), 3, "counted from the offset");
        assert_eq!(
            reaching(&l, 0, 5, Some("u1")),
            5,
            "never shorter than asked"
        );
        assert_eq!(reaching(&l, 8, 2, Some("u1")), 2, "a row already passed");
        assert_eq!(reaching(&l, 0, 2, Some("gone")), 2, "a row the list lacks");
        assert_eq!(reaching(&l, 0, 2, None), 2);
        assert_eq!(reaching(&l, 0, MAX_PAGE + 1, None), MAX_PAGE);
    }

    /// An offset past the end is an empty last page, not a panic: the list
    /// may have been rebuilt shorter since the client counted.
    #[test]
    fn an_offset_past_the_end_is_an_empty_last_page() {
        let l = list(3);
        let (rows, next) = page(&l, 10, 5);
        assert!(rows.is_empty());
        assert_eq!(next, None);
    }

    #[test]
    fn a_list_is_found_again_only_under_its_own_key() {
        let cache = ResultCache::default();
        cache.put(key("a", "c1"), Arc::new(list(2)));
        assert_eq!(cache.get(&key("a", "c1")).map(|l| l.len()), Some(2));
        assert!(
            cache.get(&key("a", "c2")).is_none(),
            "the index moved: a new commit is a new list"
        );
        assert!(cache.get(&key("b", "c1")).is_none());
    }

    /// The least recently used list goes first, so the searches a person
    /// is paging through stay while ones left behind make room.
    #[test]
    fn it_keeps_the_most_recently_used_lists() {
        let cache = ResultCache::default();
        for i in 0..CAPACITY {
            cache.put(key(&format!("q{i}"), "c"), Arc::new(list(1)));
        }
        assert!(cache.get(&key("q0", "c")).is_some());
        cache.put(key("new", "c"), Arc::new(list(1)));
        assert!(cache.get(&key("q0", "c")).is_some(), "just used, so kept");
        assert!(
            cache.get(&key("q1", "c")).is_none(),
            "least recently used, so gone"
        );
    }
}
