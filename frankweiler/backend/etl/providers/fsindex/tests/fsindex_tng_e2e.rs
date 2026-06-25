//! End-to-end scan of the checked-in TNG-themed directory tree.
//!
//! Points `extract::fetch` at the `fsindex_tng/` fixture (the same tree
//! `materialize_tng_root.sh` drops into the dev/e2e data root as
//! `fsindex_scan/`) and asserts the landed `files` rows: the right entry
//! set, the `'*.tmp'` cascade-ignore taking effect, and `.fsindex.yaml`
//! never appearing as a content row.
//!
//! fsindex is extract-only — there is no rendered Markdown to check, so this
//! is the TNG-fixture analogue of the other providers' `*_e2e` tests, scoped
//! to the raw store the scan produces.

use std::collections::BTreeSet;
use std::path::PathBuf;

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_fsindex::extract::{self, FetchOptions, RawDb};
use sqlx::Row;
use tempfile::TempDir;

/// The fixture dir, from the env var the BUILD sets (workspace-relative path,
/// resolved against the test's runfiles cwd).
fn fixture_dir() -> PathBuf {
    let dir = std::env::var("FSINDEX_TNG_DIR")
        .expect("FSINDEX_TNG_DIR must be set by the BUILD rule's env");
    PathBuf::from(dir)
}

/// Recursively copy `src` into `dst`, **dereferencing symlinks** (each file's
/// content is copied as a real regular file). bazel stages runfiles as
/// symlinks; fsindex correctly distinguishes a symlink from a regular file, so
/// scanning the runfiles tree directly would index every fixture file as a
/// symlink. Copying it first reproduces what a real user's directory looks
/// like on disk.
fn copy_deref(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `metadata()` follows symlinks, so a runfiles symlink-to-dir reads as
        // a dir and a symlink-to-file as a file.
        if std::fs::metadata(&from).unwrap().is_dir() {
            copy_deref(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

async fn file_ids(db_path: &std::path::Path) -> Vec<(String, String)> {
    let db = RawDb::open(db_path).await.unwrap();
    let rows = sqlx::query("SELECT id, kind FROM files ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    rows.into_iter()
        .map(|r| {
            (
                r.try_get::<String, _>("id").unwrap(),
                r.try_get::<String, _>("kind").unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn scans_tng_tree() {
    let staged = fixture_dir();
    assert!(
        staged.join("captains_log.txt").exists(),
        "fixture not staged at {}",
        staged.display()
    );
    // Copy the fixture into a tempdir as real files (deref the runfiles
    // symlinks), then scan that. Keep the doltlite db OUT of the scanned tree.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("tree");
    copy_deref(&staged, &root);
    let db_path = tmp.path().join("fsindex.doltlite_db");

    let summary = extract::fetch(FetchOptions {
        db_path: db_path.clone(),
        db: None,
        source_name: "fsindex-tng".to_string(),
        root: root.clone(),
        target_doltlite_branch: None,
        // Read-only against the fixture — never write breadcrumbs into it.
        no_stamp: true,
        progress: Progress::noop(),
        control: ExtractControl::default(),
    })
    .await
    .expect("scan TNG tree");

    assert_eq!(summary.errors, 0, "no walker errors");
    assert_eq!(summary.files_reused, 0, "first scan: nothing cached");
    assert_eq!(summary.stamped_directories, 0, "no_stamp=true");
    // root (D) + bridge (D) + holodeck (D) = 3 dirs;
    // captains_log, crew_manifest, bridge/viewscreen, holodeck/program_picard
    // = 4 files. helm.tmp is ignored; .fsindex.yaml is metadata, not a row.
    assert_eq!(summary.dirs, 3, "root + bridge + holodeck");
    assert_eq!(summary.files_hashed, 4);
    assert_eq!(summary.symlinks, 0);
    assert_eq!(summary.entries_scanned, 7);

    let rows = file_ids(&db_path).await;
    let ids: BTreeSet<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    let expected: BTreeSet<&str> = [
        "",
        "bridge",
        "bridge/viewscreen.txt",
        "captains_log.txt",
        "crew_manifest.txt",
        "holodeck",
        "holodeck/program_picard.txt",
    ]
    .into_iter()
    .collect();
    assert_eq!(ids, expected, "indexed entry set");

    // The cascade ignore + breadcrumb exclusion held.
    assert!(
        !ids.iter().any(|id| id.ends_with(".tmp")),
        "'*.tmp' files must be ignored"
    );
    assert!(
        !ids.iter().any(|id| id.ends_with(".fsindex.yaml")),
        ".fsindex.yaml is scanner metadata, never a content row"
    );
}
