//! Reading a raw store a page of buckets at a time, so what a render
//! holds in memory follows the page and not the size of the source.
//!
//! A page is a run of bucket keys: on a full walk, the next keys of the
//! table in key order (keyset pagination, one ordered range scan); on a
//! narrowed run, the next keys of the stale set. The provider loads what
//! hangs off a page's keys with one bound JSON array, renders it, and
//! drops it before asking for the next. Within a page, [`batches`] cuts
//! the blobs it loads by bytes. Why a page and not one bucket or all of
//! them, and how the sizes were picked: `docs/dev/data_architecture_parse_and_render.md`
//! §"Memory: a page at a time".

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// How much a render holds at once: `buckets` per page of rows, and
/// `bytes` of blobs per batch inside a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSize {
    pub buckets: usize,
    pub bytes: u64,
}

impl PageSize {
    pub const DEFAULT: PageSize = PageSize {
        buckets: 500,
        bytes: 64 * 1024 * 1024,
    };
}

/// The raw table and text column whose values are a render's bucket keys.
#[derive(Debug, Clone, Copy)]
pub struct KeyColumn {
    pub table: &'static str,
    pub column: &'static str,
}

/// The bucket keys a run renders, a page at a time.
pub struct BucketPages {
    keys: Keys,
    page: usize,
}

enum Keys {
    /// Every key the table holds, in order; `after` is the last key handed out.
    Walk {
        key: KeyColumn,
        after: Option<String>,
        done: bool,
    },
    /// A list the run was handed, sorted.
    Listed(std::vec::IntoIter<String>),
}

impl BucketPages {
    /// Every distinct non-null key in `key`, in order.
    pub fn walk(key: KeyColumn, page: usize) -> Self {
        BucketPages {
            keys: Keys::Walk {
                key,
                after: None,
                done: false,
            },
            page: page.max(1),
        }
    }

    /// `keys`, sorted and each once — a key no row holds any more comes
    /// back too, so the provider can say its bucket built nothing.
    pub fn listed(keys: impl IntoIterator<Item = String>, page: usize) -> Self {
        let sorted: Vec<String> = keys
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        BucketPages {
            keys: Keys::Listed(sorted.into_iter()),
            page: page.max(1),
        }
    }

    /// The next page of keys, or `None` once every key has been handed
    /// out. Each call is one query that is finished before it returns,
    /// so the pool's one connection is free between pages.
    pub async fn next(&mut self, pool: &SqlitePool) -> Result<Option<Vec<String>>> {
        let page = self.page;
        match &mut self.keys {
            Keys::Listed(rest) => {
                let keys: Vec<String> = rest.by_ref().take(page).collect();
                Ok((!keys.is_empty()).then_some(keys))
            }
            Keys::Walk { done: true, .. } => Ok(None),
            Keys::Walk { key, after, done } => {
                let KeyColumn { table, column } = *key;
                let keys: Vec<String> = match after {
                    None => {
                        let sql = format!(
                            "SELECT DISTINCT {column} FROM {table}
                              WHERE {column} IS NOT NULL ORDER BY {column} LIMIT ?1"
                        );
                        // Audited: `table` and `column` are `&'static str`
                        // at every callsite; the limit is bound.
                        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                            .bind(page as i64)
                            .fetch_all(pool)
                            .await
                    }
                    Some(last) => {
                        let sql = format!(
                            "SELECT DISTINCT {column} FROM {table}
                              WHERE {column} > ?1 ORDER BY {column} LIMIT ?2"
                        );
                        // Audited: as above; the last key is bound.
                        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                            .bind(last.as_str())
                            .bind(page as i64)
                            .fetch_all(pool)
                            .await
                    }
                }
                .with_context(|| format!("page of {table}.{column} keys"))?;
                if keys.len() < page {
                    *done = true;
                }
                match keys.last() {
                    Some(last) => *after = Some(last.clone()),
                    None => return Ok(None),
                }
                Ok(Some(keys))
            }
        }
    }
}

/// `weights` split, in order, into runs that sum to at most `budget`;
/// one heavier than the budget runs alone. What a page's blob loads
/// are cut by.
pub fn batches(weights: &[u64], budget: u64) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut sum: u64 = 0;
    for (i, &w) in weights.iter().enumerate() {
        if i > start && sum.saturating_add(w) > budget {
            out.push(start..i);
            start = i;
            sum = 0;
        }
        sum = sum.saturating_add(w);
    }
    if start < weights.len() {
        out.push(start..weights.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    #[test]
    fn batches_stay_under_the_budget_and_lose_nothing() {
        assert_eq!(batches(&[], 10), Vec::<Range<usize>>::new());
        assert_eq!(batches(&[3, 3, 3, 3], 6), vec![0..2, 2..4]);
        assert_eq!(batches(&[3, 3, 3], 10), vec![0..3]);
        // One over the budget runs alone rather than being dropped.
        assert_eq!(batches(&[2, 50, 2], 10), vec![0..1, 1..2, 2..3]);
        assert_eq!(batches(&[0, 0, 0], 0), vec![0..3]);
    }

    async fn pool_with_keys(keys: &[Option<&str>]) -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        for k in keys {
            sqlx::query("INSERT INTO t (k) VALUES (?)")
                .bind(k)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn drain(mut pages: BucketPages, pool: &SqlitePool) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        while let Some(page) = pages.next(pool).await.unwrap() {
            out.push(page);
        }
        out
    }

    /// A walk hands out every key once, in order, never splits a key's
    /// rows across two pages, and skips the rows with no key.
    #[tokio::test]
    async fn a_walk_hands_out_each_key_once_in_order() {
        let pool = pool_with_keys(&[
            Some("c"),
            Some("a"),
            None,
            Some("b"),
            Some("a"),
            Some("e"),
            Some("d"),
            Some("c"),
        ])
        .await;
        let key = KeyColumn {
            table: "t",
            column: "k",
        };
        assert_eq!(
            drain(BucketPages::walk(key, 2), &pool).await,
            vec![vec!["a", "b"], vec!["c", "d"], vec!["e"]]
        );
        // A table that ends on a full page still ends.
        assert_eq!(
            drain(BucketPages::walk(key, 5), &pool).await,
            vec![vec!["a", "b", "c", "d", "e"]]
        );
        let empty = pool_with_keys(&[]).await;
        assert!(drain(BucketPages::walk(key, 2), &empty).await.is_empty());
    }

    #[tokio::test]
    async fn a_list_is_handed_out_sorted_and_once() {
        let pool = pool_with_keys(&[]).await;
        let listed = BucketPages::listed(["z", "x", "y", "x"].map(String::from), 2);
        assert_eq!(drain(listed, &pool).await, vec![vec!["x", "y"], vec!["z"]]);
    }
}
