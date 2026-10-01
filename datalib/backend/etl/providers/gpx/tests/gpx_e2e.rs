//! End-to-end over the fixture files: scan → store → each file written
//! back from the store, and the diffs that edits, copies and deletions
//! leave.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use datalib_etl::doltlite_raw::{commit_run, content_tables_changed, head_commit};
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl_gpx::ingest::{self, db, RawDb};

const AWAY_TEAM: &str = "enterprise/2364-04-13 away team.gpx";
const BOZEMAN: &str = "shuttlecraft/2367-06-01 Bozeman.gpx";

fn fixture_dir() -> PathBuf {
    let rel = std::env::var("GPX_FIXTURE_DIR").expect("GPX_FIXTURE_DIR must be set by the build");
    PathBuf::from(rel)
}

fn gpx_files_under(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "gpx") {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

struct Harness {
    _tmp: tempfile::TempDir,
    raw_dir: PathBuf,
    root: PathBuf,
    /// One handle for every scan and every assertion: a second
    /// connection to the store makes the first one's commit fail.
    db: RawDb,
}

impl Harness {
    /// A writable copy of the fixture files.
    async fn new() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("gpx");
        copy_tree(&fixture_dir(), &root)?;
        let raw_dir = tmp.path().join("raw");
        std::fs::create_dir_all(&raw_dir)?;
        let db = RawDb::open(&ingest::db_path_for(&raw_dir)).await?;
        Ok(Self {
            _tmp: tmp,
            raw_dir,
            root,
            db,
        })
    }

    async fn scan(&self) -> Result<ingest::FetchSummary> {
        let cache = FingerprintCache::open(&self.raw_dir.join("fingerprints.sqlite")).await?;
        ingest::fetch(ingest::FetchOptions {
            db: self.db.clone(),
            root: self.root.clone(),
            ignore: vec![],
            cache,
            progress: datalib_etl::progress::Progress::noop(),
        })
        .await
    }

    /// Scan and commit, returning the commit.
    async fn scan_and_commit(&self, msg: &str) -> Result<(ingest::FetchSummary, String)> {
        let s = self.scan().await?;
        commit_run(self.db.pool(), msg).await?;
        let head = head_commit(self.db.pool()).await?.expect("committed");
        Ok((s, head))
    }

    async fn rebuild(&self, rel: &str) -> Result<Option<String>> {
        let mut conn = self.db.pool().acquire().await?;
        db::rebuild(&mut conn, rel).await
    }

    async fn count(&self, table: &str) -> Result<i64> {
        // Audited: test-only, and every caller passes a table-name literal.
        Ok(
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(self.db.pool())
                .await?,
        )
    }

    /// `added` / `modified` / `removed` counts for one table between two
    /// commits; tables that did not move are absent.
    async fn diff(&self, from: &str, to: &str) -> Result<BTreeMap<String, BTreeMap<String, i64>>> {
        let mut out = BTreeMap::new();
        for t in ingest::schema_raw::ALL {
            // Audited: the table name is a `&'static str` and both refs are
            // commit hashes this test read back from the store.
            let rows: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT diff_type, count(*) FROM dolt_diff_{}('{from}', '{to}') GROUP BY diff_type",
                t.name
            )))
            .fetch_all(self.db.pool())
            .await?;
            if !rows.is_empty() {
                out.insert(t.name.to_string(), rows.into_iter().collect());
            }
        }
        Ok(out)
    }
}

fn counts(pairs: &[(&str, &[(&str, i64)])]) -> BTreeMap<String, BTreeMap<String, i64>> {
    pairs
        .iter()
        .map(|(t, kinds)| {
            (
                t.to_string(),
                kinds.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
            )
        })
        .collect()
}

/// The claim the store exists for: every file comes back from its rows
/// alone, byte for byte.
#[tokio::test]
async fn every_fixture_file_comes_back_byte_for_byte() -> Result<()> {
    let h = Harness::new().await?;
    let s = h.scan().await?;
    let files = gpx_files_under(&h.root);
    assert_eq!(files.len(), 5);
    assert_eq!((s.read, s.exact, s.errors), (5, 5, 0), "{s:?}");
    for rel in &files {
        let original = std::fs::read_to_string(h.root.join(rel))?;
        let back = h.rebuild(rel).await?.expect("stored");
        assert_eq!(back, original, "{rel}");
    }
    assert_eq!(h.count("gpx_rtepts").await?, 3);
    assert_eq!(h.count("gpx_wpts").await?, 2);
    Ok(())
}

/// Two scans of a tree nobody touched leave no content diff: the
/// second reads no file and writes no row.
#[tokio::test]
async fn a_second_scan_of_an_unchanged_tree_moves_no_content_row() -> Result<()> {
    let h = Harness::new().await?;
    let (_, first) = h.scan_and_commit("first").await?;
    let (s, second) = h.scan_and_commit("second").await?;
    assert_eq!((s.read, s.unchanged), (0, 5));
    assert_eq!(
        content_tables_changed(h.db.pool(), &first, &second).await?,
        Vec::<String>::new()
    );
    Ok(())
}

/// One point's elevation changes: one point row out, one in, one member
/// row repointed, and the file's own row. Nothing else.
#[tokio::test]
async fn an_edited_point_is_a_small_diff() -> Result<()> {
    let h = Harness::new().await?;
    let (_, before) = h.scan_and_commit("before").await?;
    let path = h.root.join(AWAY_TEAM);
    let src = std::fs::read_to_string(&path)?;
    std::fs::write(&path, src.replacen("<ele>15.0</ele>", "<ele>15.5</ele>", 1))?;
    let (s, after) = h.scan_and_commit("after").await?;
    assert_eq!((s.read, s.points_added, s.points_removed), (1, 1, 1));
    assert_eq!(
        h.diff(&before, &after).await?,
        counts(&[
            ("gpx_files", &[("modified", 1)]),
            ("gpx_trkpts", &[("added", 1), ("removed", 1)]),
            ("gpx_trkseg_trkpts", &[("modified", 1)]),
        ])
    );
    assert_eq!(
        h.rebuild(AWAY_TEAM).await?,
        Some(std::fs::read_to_string(&path)?)
    );
    Ok(())
}

/// Dropping a stray point from a timed track removes its two rows and
/// moves no other point's.
#[tokio::test]
async fn a_deleted_point_moves_no_other_row() -> Result<()> {
    let h = Harness::new().await?;
    let (_, before) = h.scan_and_commit("before").await?;
    let path = h.root.join(BOZEMAN);
    let src = std::fs::read_to_string(&path)?;
    let line = src
        .lines()
        .find(|l| l.contains("<ele>1461</ele>"))
        .unwrap()
        .to_string();
    std::fs::write(&path, src.replace(&format!("{line}\n"), ""))?;
    let (_, after) = h.scan_and_commit("after").await?;
    assert_eq!(
        h.diff(&before, &after).await?,
        counts(&[
            ("gpx_files", &[("modified", 1)]),
            ("gpx_trkpts", &[("removed", 1)]),
            ("gpx_trkseg_trkpts", &[("removed", 1)]),
        ])
    );
    Ok(())
}

/// A second copy of a file shares every point row with the first; only
/// the per-file rows are new, and each copy comes back on its own.
#[tokio::test]
async fn a_copied_file_adds_no_point_rows() -> Result<()> {
    let h = Harness::new().await?;
    h.scan().await?;
    let points = h.count("gpx_trkpts").await? + h.count("gpx_wpts").await?;
    let members = h.count("gpx_trkseg_trkpts").await?;
    std::fs::create_dir_all(h.root.join("backup"))?;
    std::fs::copy(h.root.join(AWAY_TEAM), h.root.join("backup/away.gpx"))?;
    let s = h.scan().await?;
    assert_eq!((s.read, s.points_added), (1, 0));
    assert_eq!(
        h.count("gpx_trkpts").await? + h.count("gpx_wpts").await?,
        points
    );
    assert_eq!(h.count("gpx_trkseg_trkpts").await?, members + 6);
    let original = std::fs::read_to_string(h.root.join(AWAY_TEAM))?;
    assert_eq!(h.rebuild("backup/away.gpx").await?, Some(original.clone()));
    assert_eq!(h.rebuild(AWAY_TEAM).await?, Some(original));
    Ok(())
}

/// A deleted file takes its own rows and the points no other file
/// holds; a point its copy still holds stays.
#[tokio::test]
async fn a_deleted_file_takes_only_the_points_no_one_else_holds() -> Result<()> {
    let h = Harness::new().await?;
    std::fs::create_dir_all(h.root.join("backup"))?;
    std::fs::copy(h.root.join(AWAY_TEAM), h.root.join("backup/away.gpx"))?;
    h.scan().await?;
    let trkpts = h.count("gpx_trkpts").await?;
    std::fs::remove_file(h.root.join(AWAY_TEAM))?;
    std::fs::remove_file(h.root.join(BOZEMAN))?;
    let s = h.scan().await?;
    assert_eq!((s.removed, s.points_removed), (2, 7), "{s:?}");
    assert_eq!(h.count("gpx_trkpts").await?, trkpts - 7);
    assert_eq!(h.rebuild(AWAY_TEAM).await?, None);
    assert!(h.rebuild("backup/away.gpx").await?.is_some());
    let leftover: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gpx_trksegs s JOIN gpx_files f USING (file_key) WHERE f.path = ?",
    )
    .bind(BOZEMAN)
    .fetch_one(h.db.pool())
    .await?;
    assert_eq!(leftover, 0);
    Ok(())
}

/// A renamed file is the same points under a new path.
#[tokio::test]
async fn a_moved_file_keeps_its_points() -> Result<()> {
    let h = Harness::new().await?;
    h.scan().await?;
    std::fs::rename(h.root.join(BOZEMAN), h.root.join("bozeman.gpx"))?;
    let s = h.scan().await?;
    assert_eq!(
        (s.read, s.removed, s.points_added, s.points_removed),
        (1, 1, 0, 0)
    );
    assert_eq!(h.rebuild(BOZEMAN).await?, None);
    assert!(h.rebuild("bozeman.gpx").await?.is_some());
    Ok(())
}

/// A file that is not GPX, or not XML, is a problem row naming it; the
/// rest of the tree is ingested, and it is tried again next run.
#[tokio::test]
async fn a_file_that_will_not_parse_is_a_problem_not_a_failed_run() -> Result<()> {
    let h = Harness::new().await?;
    std::fs::write(h.root.join("broken.gpx"), "<gpx><trk>")?;
    std::fs::write(h.root.join("track.gpx"), "<kml><Document/></kml>")?;
    let s = h.scan().await?;
    assert_eq!((s.read, s.errors), (5, 2));
    let keys: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
        .fetch_all(h.db.pool())
        .await?;
    assert_eq!(
        keys,
        ["record:gpx_files:broken.gpx", "record:gpx_files:track.gpx"]
    );
    let s = h.scan().await?;
    assert_eq!((s.read, s.errors), (0, 2), "not stamped, so read again");
    Ok(())
}
