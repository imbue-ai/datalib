//! What a scan could not record is a `problems` row that the next clean
//! scan clears, and one bad folder costs that folder, not the run.

use std::fs;
use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_fsindex::ingest::{self, FetchOptions, FetchSummary, RawDb};
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    db: RawDb,
    cache: FingerprintCache,
}

impl Env {
    async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("tree");
        fs::create_dir(&root).unwrap();
        let db = RawDb::open(&tmp.path().join("fsindex.doltlite_db"))
            .await
            .unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite"))
            .await
            .unwrap();
        Self {
            _tmp: tmp,
            root,
            db,
            cache,
        }
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    async fn scan(&self, stamp: bool) -> FetchSummary {
        ingest::fetch(FetchOptions {
            db: self.db.clone(),
            source_id: "enterprise".to_string(),
            root: self.root.clone(),
            target_doltlite_branch: None,
            cache: self.cache.clone(),
            no_stamp: !stamp,
            progress: Progress::noop(),
            control: DownloadControl::default(),
        })
        .await
        .unwrap()
    }

    async fn problem_keys(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }

    async fn ids(&self, table: &str) -> Vec<String> {
        let sql = match table {
            "files" => "SELECT id FROM files ORDER BY id",
            _ => "SELECT id FROM dirs ORDER BY id",
        };
        sqlx::query_scalar(sql)
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }
}

/// A folder whose options file will not parse is a row on that folder,
/// and with stamping on, the folders around it are still stamped; the
/// row goes once the file parses.
#[tokio::test]
async fn a_bad_options_file_costs_its_folder_only() {
    let env = Env::new().await;
    env.write(".fsindex.yaml", "stamp_me_with_uuid: true\n");
    env.write("bridge/viewscreen.txt", "on screen");
    env.write("engineering/.fsindex.yaml", "ignore: [unclosed\n");
    env.write("engineering/warp_core.txt", "dilithium");

    let s = env.scan(true).await;
    assert_eq!(s.errors, 1, "{s:?}");
    assert_eq!(env.problem_keys().await, ["record:dirs:engineering"]);
    let stamped: Vec<String> =
        sqlx::query_scalar("SELECT id FROM dirs WHERE identity_uuid IS NOT NULL ORDER BY id")
            .fetch_all(env.db.pool())
            .await
            .unwrap();
    assert_eq!(stamped, ["", "bridge"]);

    env.write("engineering/.fsindex.yaml", "ignore: []\n");
    let s = env.scan(true).await;
    assert_eq!(s.errors, 0, "{s:?}");
    assert_eq!(env.problem_keys().await, Vec::<String>::new());
}

/// A folder that would not list is a row, and the next scan lists it
/// again even though nothing in it moved — before, its fingerprint said
/// "unchanged, no children", so it read empty with no error until
/// something in it changed.
#[tokio::test]
async fn a_folder_that_would_not_list_is_listed_again() {
    let env = Env::new().await;
    env.write("holodeck/program_picard.txt", "Dixon Hill");
    let holodeck = env.root.join("holodeck");
    set_mode(&holodeck, 0o000);
    if fs::read_dir(&holodeck).is_ok() {
        // Root lists through any mode; CI's container runs as root.
        set_mode(&holodeck, 0o755);
        return;
    }
    env.scan(false).await;
    set_mode(&holodeck, 0o755);
    assert_eq!(env.problem_keys().await, ["record:dirs:holodeck"]);
    assert_eq!(env.ids("files").await, Vec::<String>::new());

    let s = env.scan(false).await;
    assert_eq!(s.errors, 0, "{s:?}");
    assert_eq!(env.ids("files").await, ["holodeck/program_picard.txt"]);
    assert_eq!(env.problem_keys().await, Vec::<String>::new());
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
