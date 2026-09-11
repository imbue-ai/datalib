//! Reading `files` and `dirs` out of doltlite stores.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::analyze::Explained;
use crate::model::Entry;

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

/// Which of fsindex's two entry tables a read goes to. `dirs` is a few
/// percent the size of `files` and a directory's digest covers its
/// whole subtree, so anything that can be answered from `dirs` alone
/// is answered there first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Files,
    Dirs,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Table::Files => "files",
            Table::Dirs => "dirs",
        }
    }

    /// The row-shape both tables are read through: `dirs` has no
    /// `kind` column and `files` has no `entries`, so each side fills
    /// in the constant the other one stores.
    fn columns(self, prefix: &str) -> String {
        match self {
            Table::Files => format!(
                "{prefix}id AS {prefix}id, {prefix}kind AS {prefix}kind, \
                 {prefix}size AS {prefix}size, 0 AS {prefix}entries, \
                 hex({prefix}blake3) AS {prefix}hash"
            ),
            Table::Dirs => format!(
                "{prefix}id AS {prefix}id, 'dir' AS {prefix}kind, \
                 {prefix}size AS {prefix}size, {prefix}entries AS {prefix}entries, \
                 hex({prefix}blake3) AS {prefix}hash"
            ),
        }
    }

    fn for_kind(kind: &str) -> Table {
        if kind == "dir" {
            Table::Dirs
        } else {
            Table::Files
        }
    }
}

fn entry_from_row(row: &sqlx::sqlite::SqliteRow, prefix: &str) -> Entry {
    let col = |name: &str| format!("{prefix}{name}");
    Entry {
        path: row
            .try_get::<Option<String>, _>(col("id").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
        kind: row
            .try_get::<Option<String>, _>(col("kind").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
        size: row
            .try_get::<Option<i64>, _>(col("size").as_str())
            .ok()
            .flatten()
            .unwrap_or(0),
        digest: row
            .try_get::<Option<String>, _>(col("hash").as_str())
            .ok()
            .flatten()
            .unwrap_or_default(),
        entries: row
            .try_get::<Option<i64>, _>(col("entries").as_str())
            .ok()
            .flatten()
            .unwrap_or(0),
    }
}

/// Subtrees to leave out of the `files` diff, one list of path
/// prefixes per side, because the `dirs` diff already explained them:
/// a directory that moved or was copied whole keeps its tree-hash, and
/// the tree-hash covers every row beneath it.
///
/// Doltlite pushes no predicate into `dolt_diff_<t>` (measured: a
/// key-range filter costs the same walk as none), so this saves the
/// rows crossing into Rust, not the engine's work.
#[derive(Debug, Clone, Default)]
pub struct Skip {
    pub left: Vec<String>,
    pub right: Vec<String>,
}

/// Each excluded prefix is one more `AND` term in the statement, and
/// sqlite caps expression depth at 1000. Beyond this many of each
/// list, the rest of the interiors are fetched and rolled up the
/// ordinary way — never one side of a move without the other.
const MAX_SKIPPED_PER_LIST: usize = 100;

impl Skip {
    pub fn new(explained: &Explained) -> Self {
        let take = |list: &[String]| -> Vec<String> {
            list.iter()
                .filter(|p| !p.is_empty())
                .take(MAX_SKIPPED_PER_LIST)
                .cloned()
                .collect()
        };
        let moves: Vec<&(String, String)> = explained
            .moves
            .iter()
            .filter(|(src, dst)| !src.is_empty() && !dst.is_empty())
            .take(MAX_SKIPPED_PER_LIST)
            .collect();
        let mut left: Vec<String> = moves.iter().map(|(src, _)| src.clone()).collect();
        let mut right: Vec<String> = moves.iter().map(|(_, dst)| dst.clone()).collect();
        left.extend(take(&explained.left_copies));
        right.extend(take(&explained.right_copies));
        Skip { left, right }
    }

    fn clause(&self) -> String {
        // A prefix `p` owns the ids in [`p/`, `p0`): `0` is the byte
        // after `/`. `IS NULL` keeps the other side's rows, whose id on
        // this side is absent.
        let term = |column: &str| format!("({column} IS NULL OR {column} < ? OR {column} >= ?)");
        let mut terms: Vec<String> = Vec::new();
        terms.extend(self.left.iter().map(|_| term("from_id")));
        terms.extend(self.right.iter().map(|_| term("to_id")));
        if terms.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", terms.join(" AND "))
        }
    }

    fn bounds(&self) -> impl Iterator<Item = String> + '_ {
        self.left
            .iter()
            .chain(self.right.iter())
            .flat_map(|p| [format!("{p}/"), format!("{p}0")])
    }
}

pub async fn fetch_diff(
    pool: &SqlitePool,
    table: Table,
    from: &Commit,
    to: &Commit,
    skip: &Skip,
) -> Result<crate::model::Diff> {
    // Audited for `AssertSqlSafe`: both commits are `Commit`, which
    // only parses from ASCII hex, and the table-valued function takes
    // them as literals rather than bindable parameters; the table name
    // and column list are `Table`'s own constants; every skipped prefix
    // is bound.
    let sql = format!(
        "SELECT diff_type, {}, {} FROM dolt_diff_{}('{from}','{to}'){}",
        table.columns("from_"),
        table.columns("to_"),
        table.name(),
        skip.clause(),
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for bound in skip.bounds() {
        query = query.bind(bound);
    }
    let rows = query
        .fetch_all(pool)
        .await
        .with_context(|| format!("dolt_diff_{}", table.name()))?;

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

/// Every row of both tables at `commit`. Only for `--full-tree`.
pub async fn load_side(pool: &SqlitePool, commit: &Commit) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for table in [Table::Dirs, Table::Files] {
        // Audited for `AssertSqlSafe`: `commit` is hex by construction
        // and the rest is `Table`'s own constants.
        let sql = format!(
            "SELECT {} FROM dolt_at_{}('{commit}')",
            table.columns(""),
            table.name()
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await
            .context("load full tree")?;
        out.extend(rows.iter().map(|r| entry_from_row(r, "")));
    }
    Ok(out)
}

/// Rows at or above `threshold` bytes — the duplicate candidates.
///
/// A full scan of both tables. Neither carries an index on `size`, so
/// the filter saves transfer and grouping work, not the scan.
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
    let mut out = Vec::new();
    for table in [Table::Dirs, Table::Files] {
        // Audited for `AssertSqlSafe`: `commit` is hex by construction,
        // `threshold` is an i64 rendered by Rust, not caller text, and
        // the rest is `Table`'s own constants.
        let sql = format!(
            "SELECT {} FROM dolt_at_{}('{commit}') WHERE size >= {threshold}",
            table.columns(""),
            table.name()
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await
            .context("duplicate candidates")?;
        out.extend(rows.iter().map(|r| entry_from_row(r, "")));
    }
    Ok(out)
}

/// digest -> a path holding it at `commit`, for every digest found.
/// Each `(kind, digest)` is looked up in the table its kind lives in,
/// so a directory digest never costs the `files` scan.
pub async fn lookup_digests(
    pool: &SqlitePool,
    commit: &Commit,
    wanted: &std::collections::BTreeSet<(String, String)>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut found = std::collections::BTreeMap::new();
    if wanted.is_empty() {
        return Ok(found);
    }
    for (_, digest) in wanted {
        if digest.is_empty() || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("refusing to interpolate non-hex digest {digest:?}");
        }
    }

    for table in [Table::Dirs, Table::Files] {
        let digests: Vec<&String> = wanted
            .iter()
            .filter(|(kind, _)| Table::for_kind(kind) == table)
            .map(|(_, digest)| digest)
            .collect();
        if digests.is_empty() {
            continue;
        }
        if table == Table::Files {
            tracing::info!(
                commit = %commit, digests = digests.len(),
                "scanning the corpus for unmatched digests — `files` has no blake3 index, \
                 so this is a full scan per chunk"
            );
        }
        for chunk in digests.chunks(400) {
            let list = chunk
                .iter()
                .map(|d| format!("'{d}'"))
                .collect::<Vec<_>>()
                .join(",");
            // Audited for `AssertSqlSafe`: every element was checked to
            // be ASCII hex above, so the list holds only [0-9a-fA-F]
            // inside quotes, `commit` is hex by construction, and the
            // table name is `Table`'s own constant.
            let sql = format!(
                "SELECT id, hex(blake3) AS h FROM dolt_at_{}('{commit}') \
                 WHERE hex(blake3) IN ({list})",
                table.name()
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
