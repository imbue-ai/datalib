//! Ingest the checked-in TNG fixture — two sessions and a subagent in
//! the layout `~/.claude/projects` has — then render it and read the
//! documents back.

use std::path::PathBuf;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_claude_code::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_claude_code_render::render::render;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;

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

    sqlx::query("SELECT dolt_commit('-Am', 'fixture ingest')")
        .execute(pool)
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
        RawRange::cold(),
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
