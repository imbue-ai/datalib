//! Reading `files` out of doltlite stores.
//!
//! The whole reason this is Rust: sqlx links the same doltlite
//! amalgamation the rest of the tree does
//! (`//third-party/doltlite:sqlite3`), so there is no CLI to locate, no
//! subprocess per query, and no JSON to parse back.
//!
//! ## Reading through a pin
//!
//! Every read here goes through `dolt_at_files('<commit>')`, never a
//! bare `SELECT`. A bare `SELECT` reads doltlite's **working set** —
//! the staging area — which may hold rows a scan wrote but has not
//! committed. Resolving each side to a commit hash once, up front, and
//! reading only through that pin is what makes a run reproducible even
//! if a scan is writing to the same file while we read. See
//! `docs/dev/streaming_steps.md`.
//!
//! ## Why two files need unifying
//!
//! `dolt_diff_files` is bound to the connection's *main* database and
//! resolves commit hashes only against that database's own chunk store;
//! `ATTACH` extends neither (it reports
//! `dolt_diff_files is only available in the main database`, and a
//! foreign hash comes back `ref not found`). But a `.doltlite_db` works
//! as a `file://` remote for another, so [`unify`] fetches both scans
//! into a throwaway scratch database. Neither input is opened for
//! writing or copied, and chunk dedup makes it cost roughly the novelty
//! between the two scans rather than the size of the second.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::model::Entry;

/// Open a read pool against a doltlite file.
///
/// `max_connections(1)` is mandatory, not a tuning choice: doltlite's
/// HEAD pointer, working set and active branch are per-connection, so a
/// pool that hands out two connections shows two different views of the
/// same file. `datalib_etl::doltlite_raw` documents the symptoms; this
/// crate does not depend on it because that opener also runs DDL and a
/// rescue commit, and we must not write to someone else's scan.
pub async fn open(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("sqlite uri for {}", path.display()))?
        .create_if_missing(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        // Same reason `doltlite_raw::open` disables these: retiring the
        // one connection would silently discard the session state the
        // single-connection rule exists to protect.
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

/// A commit hash as doltlite renders it: 40 lowercase hex characters.
///
/// Wrapped in a newtype because these are interpolated into SQL — the
/// table-valued functions take them as literals — so the invariant that
/// they are hex has to hold at the boundary rather than at each use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit(String);

impl Commit {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("not a commit hash: {raw:?}");
        }
        Ok(Commit(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolve a branch name, `HEAD~2`, or a raw hash to a commit.
///
/// Done against the scan's own file, before any unification, because
/// that is the only database where a ref *name* means anything.
pub async fn resolve_ref(pool: &SqlitePool, reference: &str) -> Result<Commit> {
    let raw: String = sqlx::query_scalar("SELECT dolt_hashof(?)")
        .bind(reference)
        .fetch_one(pool)
        .await
        .with_context(|| format!("resolve ref {reference:?}"))?;
    Commit::parse(&raw)
}

/// Fetch two independent scan files into one scratch database.
///
/// Returns nothing: the caller must open a **fresh** connection to
/// `scratch` afterwards. doltlite registers the per-table
/// `dolt_diff_<table>` / `dolt_at_<table>` vtabs when a connection is
/// opened, from the tables present at that moment. The scratch database
/// is empty when we open it to add the remotes, so that connection
/// never learns about `files` and every later query on it fails with
/// `no such table: dolt_diff_files` — while a second connection to the
/// same file works fine. Fetching and reading therefore cannot share a
/// connection.
pub async fn unify(scratch: &Path, left: &Path, right: &Path) -> Result<()> {
    let pool = open(scratch).await?;
    let left = std::fs::canonicalize(left).with_context(|| format!("{}", left.display()))?;
    let right = std::fs::canonicalize(right).with_context(|| format!("{}", right.display()))?;
    for (name, path) in [("dtd_left", &left), ("dtd_right", &right)] {
        sqlx::query("SELECT dolt_remote('add', ?, ?)")
            .bind(name)
            .bind(format!("file://{}", path.display()))
            .execute(&pool)
            .await
            .with_context(|| format!("add remote {name} -> {}", path.display()))?;
        sqlx::query("SELECT dolt_fetch(?)")
            .bind(name)
            .execute(&pool)
            .await
            .with_context(|| format!("fetch {name}"))?;
    }
    // Drop it on the floor; see the note above.
    pool.close().await;
    Ok(())
}

fn entry_from_row(row: &sqlx::sqlite::SqliteRow, prefix: &str) -> Entry {
    Entry {
        path: row
            .try_get::<Option<String>, _>(format!("{prefix}id").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
        kind: row
            .try_get::<Option<String>, _>(format!("{prefix}kind").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
        size: row
            .try_get::<Option<i64>, _>(format!("{prefix}size").as_str())
            .ok()
            .flatten()
            .unwrap_or(0),
        digest: row
            .try_get::<Option<String>, _>(format!("{prefix}hash").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
    }
}

/// The prolly diff between two commits, split by `diff_type`.
pub async fn fetch_diff(
    pool: &SqlitePool,
    from: &Commit,
    to: &Commit,
) -> Result<crate::model::Diff> {
    // Audited for `AssertSqlSafe`: both are `Commit`, which only parses
    // from ASCII hex, and the table-valued function takes them as
    // literals rather than bindable parameters.
    let sql = format!(
        "SELECT diff_type, \
                from_id, to_id, from_kind, to_kind, from_size, to_size, \
                hex(from_blake3) AS from_hash, hex(to_blake3) AS to_hash \
         FROM dolt_diff_files('{from}','{to}')"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("dolt_diff_files")?;

    let mut diff = crate::model::Diff::default();
    for row in &rows {
        let kind: String = row.try_get("diff_type")?;
        match kind.as_str() {
            "removed" => diff.removed.push(entry_from_row(row, "from_")),
            "added" => diff.added.push(entry_from_row(row, "to_")),
            "modified" => diff
                .modified
                .push((entry_from_row(row, "from_"), entry_from_row(row, "to_"))),
            other => bail!("unexpected diff_type {other:?}"),
        }
    }
    Ok(diff)
}

/// Every row of `files` at `commit`. Only for `--full-tree`.
pub async fn load_side(pool: &SqlitePool, commit: &Commit) -> Result<Vec<Entry>> {
    // Audited for `AssertSqlSafe`: `commit` is hex by construction.
    let sql = format!(
        "SELECT id AS from_id, kind AS from_kind, size AS from_size, \
                hex(blake3) AS from_hash \
         FROM dolt_at_files('{commit}')"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("load full tree")?;
    Ok(rows.iter().map(|r| entry_from_row(r, "from_")).collect())
}

/// Rows at or above `threshold` bytes — the duplicate candidates.
///
/// A full scan of the tree. `files` carries no index on `size` either,
/// so the filter saves transfer and grouping work, not the scan.
pub async fn duplicate_candidates(
    pool: &SqlitePool,
    commit: &Commit,
    threshold: i64,
) -> Result<Vec<Entry>> {
    if threshold <= 0 {
        return Ok(Vec::new());
    }
    tracing::info!(
        commit = %commit, threshold,
        "scanning the tree for duplicate content — this is a full corpus scan"
    );
    // Audited for `AssertSqlSafe`: `commit` is hex by construction and
    // `threshold` is an i64 rendered by Rust, not caller text.
    let sql = format!(
        "SELECT id AS from_id, kind AS from_kind, size AS from_size, \
                hex(blake3) AS from_hash \
         FROM dolt_at_files('{commit}') WHERE size >= {threshold}"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("duplicate candidates")?;
    Ok(rows.iter().map(|r| entry_from_row(r, "from_")).collect())
}

/// Where each of `digests` lives in the tree at `commit`.
///
/// The one deliberately expensive query. `files` carries no secondary
/// index on `blake3` — the provider's `STORAGE_NOTES.md` §2 measures
/// what one would cost on a TEXT-PK table — so each chunk is a whole
/// corpus scan. It runs only for digests the move pairing could not
/// already account for, and it says so, because a quiet O(corpus) scan
/// hiding behind a fast O(changes) diff is the kind of fallback this
/// repo tells you not to add silently.
pub async fn lookup_digests(
    pool: &SqlitePool,
    commit: &Commit,
    digests: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut found = std::collections::BTreeMap::new();
    if digests.is_empty() {
        return Ok(found);
    }
    for digest in digests {
        if digest.is_empty() || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("refusing to interpolate non-hex digest {digest:?}");
        }
    }
    tracing::info!(
        commit = %commit, digests = digests.len(),
        "scanning the corpus for unmatched digests — `files` has no blake3 index, \
         so this is a full scan per chunk"
    );

    let ordered: Vec<&String> = digests.iter().collect();
    for chunk in ordered.chunks(400) {
        let list = chunk
            .iter()
            .map(|d| format!("'{d}'"))
            .collect::<Vec<_>>()
            .join(",");
        // Audited for `AssertSqlSafe`: every element was checked to be
        // ASCII hex above, so the list holds only [0-9a-fA-F] inside
        // quotes, and `commit` is hex by construction.
        let sql = format!(
            "SELECT id, hex(blake3) AS h FROM dolt_at_files('{commit}') \
             WHERE hex(blake3) IN ({list})"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await
            .context("digest lookup")?;
        for row in &rows {
            let h: String = row.try_get("h")?;
            let id: Option<String> = row.try_get("id")?;
            found.entry(h).or_insert_with(|| id.unwrap_or_default());
        }
    }
    Ok(found)
}

/// `path[#ref]`, the way each side is named on the command line.
#[derive(Debug, Clone)]
pub struct SideSpec {
    pub db: PathBuf,
    pub reference: String,
}

impl FromStr for SideSpec {
    type Err = anyhow::Error;

    fn from_str(spec: &str) -> Result<Self> {
        let (db, reference) = match spec.rsplit_once('#') {
            Some((db, r)) if !r.is_empty() => (db, r),
            _ => (spec, "HEAD"),
        };
        Ok(SideSpec {
            db: PathBuf::from(db),
            reference: reference.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_spec_defaults_to_head() {
        let s: SideSpec = "scans.doltlite_db".parse().unwrap();
        assert_eq!(s.db, PathBuf::from("scans.doltlite_db"));
        assert_eq!(s.reference, "HEAD");
    }

    #[test]
    fn a_side_spec_takes_a_ref_after_a_hash() {
        let s: SideSpec = "scans.doltlite_db#nightly".parse().unwrap();
        assert_eq!(s.db, PathBuf::from("scans.doltlite_db"));
        assert_eq!(s.reference, "nightly");
    }

    #[test]
    fn only_the_last_hash_splits() {
        let s: SideSpec = "od.d/a#b.doltlite_db#main".parse().unwrap();
        assert_eq!(s.db, PathBuf::from("od.d/a#b.doltlite_db"));
        assert_eq!(s.reference, "main");
    }

    #[test]
    fn commits_must_be_hex() {
        assert!(Commit::parse("deadBEEF00").is_ok());
        assert!(Commit::parse("").is_err());
        // The shape that matters: anything that could close the quote.
        assert!(Commit::parse("abc'); DROP TABLE files;--").is_err());
    }
}
