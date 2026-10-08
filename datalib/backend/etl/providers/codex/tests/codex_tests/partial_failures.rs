//! What a run could not read is a `problems` row, and the row goes
//! once a later run reads it. The walk is the one every agent-session
//! source shares (`datalib_etl_agent_sessions`), so this covers
//! `claude_code` too.

use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl_codex::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_files::fingerprint_cache::FingerprintCache;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CODEX_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_tng")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        // Bazel's runfiles are symlinks; follow them.
        if std::fs::metadata(entry.path()).unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

struct Env {
    _td: tempfile::TempDir,
    home: PathBuf,
    db: RawDb,
    cache: FingerprintCache,
}

impl Env {
    async fn new() -> Self {
        let td = tempfile::tempdir().unwrap();
        let raw_path = td.path().join("raw");
        std::fs::create_dir_all(&raw_path).unwrap();
        let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
        let cache = FingerprintCache::open(&td.path().join("fp.sqlite"))
            .await
            .unwrap();
        let home = td.path().join("codex_home");
        Self {
            _td: td,
            home,
            db,
            cache,
        }
    }

    async fn fetch(&self) -> anyhow::Result<FetchSummary> {
        fetch(FetchOptions {
            db: self.db.clone(),
            input_path: self.home.clone(),
            cache: self.cache.clone(),
            progress: Progress::default(),
            control: DownloadControl::default(),
        })
        .await
    }

    async fn problems(&self) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, sample FROM problems ORDER BY scope_key")
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }

    async fn problem_keys(&self) -> Vec<String> {
        self.problems().await.into_iter().map(|(k, _)| k).collect()
    }
}

const BAD_ROLLOUT: &str = "sessions/2364/04/14/rollout-2364-04-14T08-00-00-bad.jsonl";

/// A home with no `sessions/` and nothing stored fails the run; once
/// something is stored, the same absence is a `listing:` row that a
/// later run with the folder back clears. A missing `archived_sessions/`
/// is never reported: only an older Codex makes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_sessions_folder_fails_only_a_first_run() {
    let env = Env::new().await;
    std::fs::create_dir_all(&env.home).unwrap();
    let err = env.fetch().await.unwrap_err();
    assert!(format!("{err:#}").contains("is not a directory"), "{err:#}");

    copy_tree(&fixture_dir(), &env.home);
    std::fs::remove_dir_all(env.home.join("archived_sessions")).unwrap();
    env.fetch().await.unwrap();
    assert_eq!(env.problem_keys().await, Vec::<String>::new());

    let moved = env.home.join("sessions-away");
    std::fs::rename(env.home.join("sessions"), &moved).unwrap();
    let s = env.fetch().await.unwrap();
    assert_eq!(s.files, 0);
    assert_eq!(env.problem_keys().await, ["listing:codex/sessions"]);
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transcripts")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(stored, 3, "what was read before is kept");

    std::fs::rename(&moved, env.home.join("sessions")).unwrap();
    env.fetch().await.unwrap();
    assert_eq!(env.problem_keys().await, Vec::<String>::new());
}

/// An entry the walk could not read, a bad line inside a rollout, and a
/// rollout that could not be opened each leave a row, and each row
/// clears when the thing is read cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_run_could_not_read_is_a_row_until_it_reads() {
    let env = Env::new().await;
    copy_tree(&fixture_dir(), &env.home);
    let dead = env.home.join("sessions/2364/04/11/dead.jsonl");
    std::os::unix::fs::symlink(env.home.join("nowhere"), &dead).unwrap();
    let bad = env.home.join(BAD_ROLLOUT);
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    let meta = r#"{"timestamp":"2364-04-14T08:00:00.000Z","type":"session_meta","payload":{"id":"bad-thread","cwd":"/Users/riker"}}"#;
    std::fs::write(&bad, format!("{meta}\nnot json\n{meta}\n")).unwrap();

    env.fetch().await.unwrap();
    let rows = env.problems().await;
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        [
            format!("file:codex/sessions:{}", &BAD_ROLLOUT["sessions/".len()..]),
            "listing:codex/sessions".to_string(),
        ],
        "the fixture's own non-JSON line is its last, so it is not a row"
    );
    assert_eq!(
        rows[0].1,
        "1 line could not be used; first: line 2, not JSON"
    );
    assert!(
        rows[1].1.starts_with("2364/04/11/dead.jsonl: "),
        "the unreadable path leads, relative to the tree: {}",
        rows[1].1
    );

    std::fs::remove_file(&dead).unwrap();
    std::fs::write(&bad, format!("{meta}\n{meta}\n")).unwrap();
    env.fetch().await.unwrap();
    assert_eq!(env.problem_keys().await, Vec::<String>::new());

    // A rollout this source must read again — its stamp forgotten — but
    // cannot open. The host cache still vouches for its bytes, so the
    // walk itself does not open it.
    let victim =
        "2364/04/13/rollout-2364-04-13T09-15-00-72fb0cda-0eb6-754e-8bfd-a7dcc8d6c8f4.jsonl";
    sqlx::query("DELETE FROM ingested_files WHERE rel_path = ?")
        .bind(victim)
        .execute(env.db.pool())
        .await
        .unwrap();
    let path = env.home.join("sessions").join(victim);
    set_mode(&path, 0o000);
    if std::fs::read(&path).is_ok() {
        // Root reads through any mode; CI's container runs as root.
        set_mode(&path, 0o644);
        return;
    }
    let s = env.fetch().await.unwrap();
    let row = format!("record:transcripts:sessions/{victim}");
    assert_eq!(s.unreadable, 1);
    assert_eq!(env.problem_keys().await, std::slice::from_ref(&row));

    // Its folder will not list now, so this run never sees the file: its
    // row stands rather than clearing.
    let folder = path.parent().unwrap();
    set_mode(folder, 0o000);
    let s = env.fetch().await;
    set_mode(folder, 0o755);
    set_mode(&path, 0o644);
    s.unwrap();
    assert_eq!(
        env.problem_keys().await,
        ["listing:codex/sessions".to_string(), row]
    );

    let s = env.fetch().await.unwrap();
    assert_eq!((s.unreadable, s.files_read), (0, 1), "retried, and read");
    assert_eq!(env.problem_keys().await, Vec::<String>::new());
}

/// A rollout that is gone takes its stamp, and the row on what its read
/// could not use, with it; the rows read from it stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_gone_rollout_takes_its_file_row_with_it() {
    let env = Env::new().await;
    copy_tree(&fixture_dir(), &env.home);
    let bad = env.home.join(BAD_ROLLOUT);
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    let meta = r#"{"timestamp":"2364-04-14T08:00:00.000Z","type":"session_meta","payload":{"id":"bad-thread","cwd":"/Users/riker"}}"#;
    std::fs::write(&bad, format!("{meta}\nnot json\n{meta}\n")).unwrap();
    env.fetch().await.unwrap();
    assert_eq!(env.problem_keys().await.len(), 1);

    std::fs::remove_file(&bad).unwrap();
    env.fetch().await.unwrap();
    assert_eq!(env.problem_keys().await, Vec::<String>::new());
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transcripts WHERE id = 'bad-thread'")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(kept, 1);
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
