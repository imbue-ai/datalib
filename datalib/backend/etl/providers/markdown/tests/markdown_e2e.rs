//! The TNG notes folder scanned into a real raw store, committed, and read
//! back by the render: what the scan keeps and skips, what a second scan
//! of an unchanged folder costs, and which notes an edit, a move and a
//! delete make the next render look at.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use datalib_etl::doltlite_raw::{commit_run, head_commit};
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_markdown::ingest::{self, RawDb};
use datalib_etl_markdown_render::render::{self, note};
use datalib_etl_render::inputs::RawRange;
use datalib_etl_render::section::join;
use datalib_sql::{test_sql, Sql};

const SOURCE: &str = "notes";
const NOW: &str = "2364-04-13T08:45:00-07:00";

fn fixture_dir() -> PathBuf {
    let rel = std::env::var("MARKDOWN_FIXTURE_DIR").expect("set by the build");
    let p = PathBuf::from(&rel);
    assert!(p.is_dir(), "fixture dir not found: {rel}");
    p
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        // Bazel's runfiles are symlinks; copying through them gives a tree
        // the test can edit.
        if std::fs::metadata(entry.path()).unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    raw_dir: PathBuf,
    root: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let raw_dir = tmp.path().join("raw");
        let root = tmp.path().join("vault");
        std::fs::create_dir_all(&raw_dir).unwrap();
        copy_tree(&fixture_dir(), &root);
        Self {
            _tmp: tmp,
            raw_dir,
            root,
        }
    }

    async fn scan(&self) -> ingest::FetchSummary {
        let db = RawDb::open(&ingest::db_path_for(&self.raw_dir))
            .await
            .unwrap();
        let cache = FingerprintCache::open(&self.raw_dir.join("fingerprints.sqlite"))
            .await
            .unwrap();
        let config: datalib_etl_markdown_config::MarkdownConfig =
            serde_json::from_value(serde_json::json!({})).unwrap();
        let summary = ingest::fetch(ingest::FetchOptions {
            db: db.clone(),
            source_id: SOURCE.into(),
            root: self.root.clone(),
            ignore: config.scan_ignores(),
            max_bytes: config.max_bytes,
            cache: cache.clone(),
            now: NOW.into(),
            progress: datalib_etl::progress::Progress::noop(),
        })
        .await
        .unwrap();
        commit_run(db.pool(), "test: markdown scan").await.unwrap();
        cache.pool().close().await;
        db.close().await;
        summary
    }

    async fn head(&self) -> Option<String> {
        let db = RawDb::open(&ingest::db_path_for(&self.raw_dir))
            .await
            .unwrap();
        let head = head_commit(db.pool()).await.unwrap();
        db.close().await;
        head
    }

    async fn query(&self, sql: Sql) -> Vec<String> {
        let db = RawDb::open(&ingest::db_path_for(&self.raw_dir))
            .await
            .unwrap();
        let out = sql.query_scalar().fetch_all(db.pool()).await.unwrap();
        db.close().await;
        out
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.root.join(rel), text).unwrap();
    }
}

fn ids(loaded: &render::Loaded) -> Vec<&str> {
    loaded.notes.iter().map(|n| n.id.as_str()).collect()
}

/// Every markdown file is one note under its path; hidden folders and
/// other files are left alone; a file that is not UTF-8 is kept, and
/// says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_stores_each_note_under_its_path() {
    let h = Harness::new();
    let s = h.scan().await;
    assert_eq!((s.files_seen, s.added, s.errors), (4, 4, 0), "{s:?}");

    let loaded = render::load(&h.raw_dir, RawRange::cold())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ids(&loaded),
        [
            "Bridge/Away teams/Roster.markdown",
            "Bridge/Warp core.md",
            "Captain's log.md",
            "Sickbay/Medical log.md",
        ]
    );
    let medical = &loaded.notes[3];
    assert!(medical.text.contains("38\u{FFFD}C"), "{}", medical.text);
    let problems = h.query(test_sql!("SELECT scope_key FROM problems")).await;
    assert_eq!(problems, ["file:markdown/files:Sickbay/Medical log.md"]);

    let (_, root) = loaded.scan.clone().unwrap();
    let log = note::render_note(SOURCE, &loaded.notes[2], Some(&root));
    let page = join(&log.sections);
    assert!(page.contains("source_path: \"Captain's log.md\""), "{page}");
    assert!(page.contains("```yaml\ntags: [log, enterprise]"), "{page}");
    assert!(
        page.contains("Our destination is planet [[Deneb IV]]"),
        "{page}"
    );
}

/// An unchanged folder costs a stat per file and commits nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_scan_of_an_unchanged_folder_commits_nothing() {
    let h = Harness::new();
    h.scan().await;
    let before = h.head().await;
    let s = h.scan().await;
    assert_eq!(
        (s.added, s.changed, s.removed, s.unchanged),
        (0, 0, 0, 4),
        "{s:?}"
    );
    assert_eq!(h.head().await, before);
}

/// An edit renders that note again, a move renders the new path and
/// removes the old, a delete removes the note; nothing else is looked at.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_a_move_and_a_delete_render_only_those_notes() {
    let h = Harness::new();
    h.scan().await;
    let cursor = h.head().await.unwrap();

    h.write("Bridge/Warp core.md", "# Warp core\n\nEjected. Again.\n");
    std::fs::rename(
        h.root.join("Captain's log.md"),
        h.root.join("Bridge/Captain's log.md"),
    )
    .unwrap();
    std::fs::remove_file(h.root.join("Bridge/Away teams/Roster.markdown")).unwrap();
    let s = h.scan().await;
    assert_eq!((s.added, s.changed, s.removed), (1, 1, 2), "{s:?}");

    // The driver found nothing stale of its own: what renders is what the
    // diff since the cursor names.
    let stale = HashSet::new();
    let range = RawRange {
        cursor: Some(&cursor),
        pin: None,
        stale: Some(&stale),
    };
    let loaded = render::load(&h.raw_dir, range).await.unwrap().unwrap();
    assert_eq!(
        ids(&loaded),
        ["Bridge/Captain's log.md", "Bridge/Warp core.md"]
    );
    assert_eq!(
        loaded.gone,
        ["Bridge/Away teams/Roster.markdown", "Captain's log.md"]
    );
    assert!(loaded.notes[1].text.contains("Ejected"));
}
