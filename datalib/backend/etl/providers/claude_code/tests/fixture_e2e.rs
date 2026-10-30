//! Ingest the checked-in TNG fixture — two sessions and a subagent in
//! the layout `~/.claude/projects` has — then render it and read the
//! documents back.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl_claude_code::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_claude_code_render::render::render;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_render::pages::PageSize;

const STANZA: &str = "claude-code";
const SESSION_1: &str = "91ad7d3f-2915-5956-995d-cba3754abadc";
const SESSION_2: &str = "16e654a6-fc21-5e23-a5a4-def9b696c409";
const AGENT_1: &str = "ae0c430478585517d";

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_CODE_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_code_tng")
}

async fn count(pool: &sqlx::SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tng_fixture_ingests_and_renders() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    let cache = FingerprintCache::open(&root.join("fp.sqlite"))
        .await
        .unwrap();
    let opts = || FetchOptions {
        db: db.clone(),
        input_path: fixture_dir(),
        cache: cache.clone(),
        progress: Progress::default(),
        control: DownloadControl::default(),
    };

    let s = fetch(opts()).await.unwrap();
    assert_eq!(
        (s.files, s.files_read),
        (3, 3),
        "two sessions and a subagent"
    );
    assert_eq!((s.transcripts, s.subagents), (3, 1));
    assert_eq!(s.malformed_lines, 1, "the non-JSON line is stepped over");
    assert_eq!((s.not_transcripts, s.unreadable), (0, 0));
    let pool = db.pool();
    assert_eq!(count(pool, "SELECT COUNT(*) FROM transcripts").await, 3);
    // Every content record is a row; the bookkeeping records are not.
    // The record's type is read off the payload: nothing promotes it.
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM records WHERE payload->>'$.type' = 'system'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM records \
             WHERE payload->>'$.type' NOT IN ('user','assistant','system')"
        )
        .await,
        0
    );
    let (title, cwd, agent): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT title, cwd, agent_id FROM transcripts WHERE id = ?")
            .bind(SESSION_1)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        title.as_deref(),
        Some("Deflector dish drift"),
        "the custom title wins over the AI one"
    );
    assert_eq!(cwd.as_deref(), Some("/Users/picard/src/enterprise"));
    assert_eq!(agent, None);
    let (title2,): (Option<String>,) = sqlx::query_as("SELECT title FROM transcripts WHERE id = ?")
        .bind(SESSION_2)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        title2.as_deref(),
        Some("Why does the replicator return cold Earl Grey? Screenshot attached."),
        "an unnamed session is titled by its first real prompt, not the harness caveat"
    );
    let agent_tid = format!("{SESSION_1}#{AGENT_1}");
    let (agent_session, agent_records): (String, i64) = sqlx::query_as(
        "SELECT t.session_id, (SELECT COUNT(*) FROM records r WHERE r.transcript_id = t.id) \
         FROM transcripts t WHERE t.id = ?",
    )
    .bind(&agent_tid)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(agent_session, SESSION_1);
    assert_eq!(agent_records, 4);
    // Payloads are JSONB; `json()` gives the text back.
    let meta: (String,) = sqlx::query_as("SELECT json(payload) FROM transcripts WHERE id = ?")
        .bind(SESSION_1)
        .fetch_one(pool)
        .await
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta.0).unwrap();
    assert_eq!(meta["cloud_session_id"], "cse_01TNGdeflector");
    assert_eq!(meta["pr_links"][0]["prNumber"], 1701);
    assert_eq!(meta["title_source"], "custom");

    // A second pass reads nothing.
    let again = fetch(opts()).await.unwrap();
    assert_eq!((again.files, again.files_read), (3, 0));

    datalib_etl::doltlite_raw::commit_run(pool, "fixture ingest")
        .await
        .unwrap();
    db.close().await;

    let out_root = root.join("render");
    let mut docs: Vec<RenderedMarkdown> = Vec::new();
    let outcome = render(
        &raw_path,
        &out_root,
        STANZA,
        &Progress::default(),
        &mut |md| {
            docs.push(md);
            Ok(())
        },
        &mut |_| Ok(()),
        RawRange::cold(),
        PageSize::DEFAULT,
        16 * 1024,
    )
    .unwrap();
    assert_eq!(outcome.rendered, 3);
    assert_eq!(docs.len(), 3);

    let title_of = |d: &RenderedMarkdown| {
        d.rows
            .iter()
            .find_map(|r| r.conversation_name.clone())
            .unwrap_or_default()
    };
    let by_title = |needle: &str| {
        docs.iter()
            .find(|d| title_of(d).contains(needle))
            .unwrap_or_else(|| panic!("no document titled like {needle:?}"))
    };
    let s1 = by_title("Deflector dish drift");
    let md = std::fs::read_to_string(&s1.md_path).unwrap();
    assert!(md.contains("Tool use: Bash"), "{md}");
    assert!(
        md.contains("Tool result: Read"),
        "the result names the tool it answers"
    );
    assert!(md.contains("phase variance: 0.31 deg"));
    assert!(
        md.contains("<details class=\"tool-group\""),
        "tool traffic folds"
    );
    assert!(md.contains("Lowered the realignment threshold"));
    assert!(
        !md.contains("42000 tokens used"),
        "harness attachments are not rendered"
    );
    let kinds: std::collections::BTreeSet<&str> = s1.rows.iter().map(|r| r.kind.as_str()).collect();
    for k in [
        "User Input",
        "LLM Thinking",
        "Tool Call",
        "Tool Result",
        "LLM Response",
    ] {
        assert!(kinds.contains(k), "missing {k} in {kinds:?}");
    }
    assert!(s1
        .rows
        .iter()
        .all(|r| r.project.as_deref() == Some("enterprise")));
    assert!(
        s1.rows
            .iter()
            .any(|r| r.source_url.as_deref() == Some("https://claude.ai/code/cse_01TNGdeflector")),
        "the bridged cloud session is the session's link"
    );

    let agent = by_title("subagent of Deflector dish drift");
    assert!(title_of(agent).starts_with("Scan the emitter logs"));
    let md = std::fs::read_to_string(&agent.md_path).unwrap();
    assert!(md.contains("Tool use: Grep"));

    let s2 = by_title("cold Earl Grey");
    let md = std::fs::read_to_string(&s2.md_path).unwrap();
    assert!(md.contains("Tool result: Bash (error)"));
    assert!(md.contains("1 image(s) not shown"));
    assert!(md.contains("claude-sonnet-5"));
}

#[derive(Default)]
struct Counts {
    lengths: Mutex<Vec<Option<u64>>>,
    done: Mutex<u64>,
}

impl ProgressSink for Counts {
    fn set_length(&self, total: Option<u64>) {
        self.lengths.lock().unwrap().push(total);
    }
    fn inc(&self, delta: u64) {
        *self.done.lock().unwrap() += delta;
    }
}

/// The Manage screen's ETA is the announced total less what was counted
/// done, over the pace it was counted. The download announced nothing,
/// so a Claude Code sync showed no ETA however long it ran.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_announces_the_files_it_reads_and_counts_them_done() {
    let td = tempfile::tempdir().unwrap();
    let raw_path = td.path().join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    let cache = FingerprintCache::open(&td.path().join("fp.sqlite"))
        .await
        .unwrap();
    let sync = |counts: Arc<Counts>| {
        fetch(FetchOptions {
            db: db.clone(),
            input_path: fixture_dir(),
            cache: cache.clone(),
            progress: Progress::new(counts),
            control: DownloadControl::default(),
        })
    };

    let first = Arc::new(Counts::default());
    sync(first.clone()).await.unwrap();
    assert_eq!(*first.lengths.lock().unwrap(), vec![Some(3)]);
    assert_eq!(*first.done.lock().unwrap(), 3);

    let unchanged = Arc::new(Counts::default());
    sync(unchanged.clone()).await.unwrap();
    assert_eq!(
        *unchanged.lengths.lock().unwrap(),
        vec![],
        "nothing to read announces no total, which is not a total of zero"
    );
    assert_eq!(*unchanged.done.lock().unwrap(), 0);
    db.close().await;
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

/// A render read every transcript and record in the store before it
/// kept the few that changed: gigabytes held for one new message. It
/// now loads a page of transcripts at a time, and on a narrowed run
/// only the ones the diff names.
#[tokio::test(flavor = "multi_thread")]
async fn a_render_reads_a_page_at_a_time_and_only_what_changed() {
    let td = tempfile::tempdir().unwrap();
    let input = td.path().join("projects");
    copy_tree(&fixture_dir(), &input);
    let raw_path = td.path().join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let cache = FingerprintCache::open(&td.path().join("fp.sqlite"))
        .await
        .unwrap();
    let sync = || async {
        let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
        fetch(FetchOptions {
            db: db.clone(),
            input_path: input.clone(),
            cache: cache.clone(),
            progress: Progress::default(),
            control: DownloadControl::default(),
        })
        .await
        .unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "sync")
            .await
            .unwrap();
        db.close().await;
    };
    let out_root = td.path().join("render");
    let render_with = |range: RawRange<'_>, progress: Progress| {
        let mut titles: Vec<String> = Vec::new();
        let mut pages: Vec<usize> = Vec::new();
        let outcome = render(
            &raw_path,
            &out_root,
            STANZA,
            &progress,
            &mut |md| {
                titles.extend(md.rows.iter().find_map(|r| r.conversation_name.clone()));
                Ok(())
            },
            &mut |buckets| {
                pages.push(buckets.len());
                Ok(())
            },
            range,
            PageSize {
                buckets: 1,
                bytes: PageSize::DEFAULT.bytes,
            },
            16 * 1024,
        )
        .unwrap();
        (outcome, titles, pages)
    };

    sync().await;
    let (cold, titles, pages) = render_with(RawRange::cold(), Progress::default());
    assert_eq!(titles.len(), 3);
    assert_eq!(
        pages,
        [0, 1, 1, 1],
        "nothing gone, then one page per transcript"
    );
    let head = cold.new_head.expect("a cold render stamps the head");

    let file = input
        .join("-Users-picard-src-enterprise")
        .join(format!("{SESSION_2}.jsonl"));
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(
        &serde_json::json!({
            "type": "user", "uuid": "0d1c5e7a-0000-4000-8000-000000000001",
            "sessionId": SESSION_2, "timestamp": "2364-04-11T15:00:00.000Z",
            "message": {"role": "user", "content": "Tea, Earl Grey, hot. Again."},
        })
        .to_string(),
    );
    text.push('\n');
    std::fs::write(&file, text).unwrap();
    sync().await;

    let counts = Arc::new(Counts::default());
    let nothing_stale = std::collections::HashSet::new();
    let (narrowed, titles, pages) = render_with(
        RawRange {
            cursor: Some(&head),
            pin: None,
            stale: Some(&nothing_stale),
        },
        Progress::new(counts.clone()),
    );
    assert_eq!((narrowed.rendered, narrowed.skipped), (1, 2));
    assert_eq!(titles.len(), 1);
    assert!(titles[0].contains("cold Earl Grey"), "{titles:?}");
    assert_eq!(pages, [0, 1]);
    assert_eq!(*counts.lengths.lock().unwrap(), vec![Some(1)]);
    assert_eq!(*counts.done.lock().unwrap(), 1);
}
