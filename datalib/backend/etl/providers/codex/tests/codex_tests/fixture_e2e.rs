//! Ingest the checked-in TNG fixture — three threads and a sub-agent in
//! the layout `~/.codex` has — then render it and read the documents
//! back. Two of Codex's formats are in there: the 0.115 threads, where
//! a `user_message` event is the only record of what the person typed,
//! and the 0.155 one, where the message tags itself and there are no
//! such events at all.

use std::path::PathBuf;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_codex::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_codex_render::render::render;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;

const STANZA: &str = "codex";
const THREAD_1: &str = "41b8781c-6a77-7fbf-9c78-8125d1a37f24";
const THREAD_2: &str = "8233ca2a-6faa-78d9-85d7-170a69389bd3";
const AGENT_1: &str = "0959dd40-b9f1-7ec2-b3d8-643ac3857878";
const THREAD_3: &str = "72fb0cda-0eb6-754e-8bfd-a7dcc8d6c8f4";

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CODEX_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_tng")
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
        (4, 4),
        "three threads under sessions/ and one under archived_sessions/; history.jsonl is not under either"
    );
    assert_eq!((s.transcripts, s.subagents), (4, 1));
    assert_eq!(s.malformed_lines, 1, "the non-JSON line is stepped over");
    assert_eq!((s.not_transcripts, s.unreadable), (0, 0));
    assert_eq!(s.records, 35 - 1 + 11 + 14 + 29);
    let pool = db.pool();
    assert_eq!(count(pool, "SELECT COUNT(*) FROM transcripts").await, 4);
    // Every line is a row; its type is read off the payload.
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM records WHERE payload->>'$.type' = 'session_meta'"
        )
        .await,
        4
    );
    // The line types only the newer Codex writes are stored like any
    // other; nothing renders them.
    for (kind, n) in [("world_state", 2), ("token_usage_record", 2)] {
        let got: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE payload->>'$.type' = ?")
                .bind(kind)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(got, n, "{kind}");
    }
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM records WHERE payload->>'$.payload.type' = 'function_call'"
        )
        .await,
        3
    );
    let (title, cwd, parent, rel): (Option<String>, Option<String>, Option<String>, String) =
        sqlx::query_as(
            "SELECT title, cwd, parent_thread_id, rel_path FROM transcripts WHERE id = ?",
        )
        .bind(THREAD_1)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        title.as_deref(),
        Some("The deflector dish drifts 0.3 degrees every hour. Find out why and fix it."),
        "a thread is titled by the first prompt the person typed"
    );
    assert_eq!(cwd.as_deref(), Some("/Users/picard/src/enterprise"));
    assert_eq!(parent, None);
    assert!(rel.starts_with("sessions/2364/04/11/rollout-"), "{rel}");
    let (agent_title, agent_parent, agent_records): (Option<String>, Option<String>, i64) =
        sqlx::query_as(
            "SELECT t.title, t.parent_thread_id, \
             (SELECT COUNT(*) FROM records r WHERE r.transcript_id = t.id) \
             FROM transcripts t WHERE t.id = ?",
        )
        .bind(AGENT_1)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        agent_title.as_deref(),
        Some("Data"),
        "a sub-agent is titled by its nickname"
    );
    assert_eq!(agent_parent.as_deref(), Some(THREAD_1));
    assert_eq!(agent_records, 11);
    let (rel2,): (String,) = sqlx::query_as("SELECT rel_path FROM transcripts WHERE id = ?")
        .bind(THREAD_2)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(rel2.starts_with("archived_sessions/"), "{rel2}");
    // Payloads are JSONB; `json()` gives the text back.
    let meta: (String,) = sqlx::query_as("SELECT json(payload) FROM transcripts WHERE id = ?")
        .bind(THREAD_1)
        .fetch_one(pool)
        .await
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta.0).unwrap();
    assert_eq!(meta["cli_version"], "0.115.0");
    assert_eq!(meta["git_branch"], "main");
    assert_eq!(meta["title_source"], "prompt");
    assert_eq!(meta["turns"], 2);
    assert_eq!(meta["models"], serde_json::json!(["gpt-5.3-codex"]));
    assert_eq!(meta["item_counts"]["custom_tool_call"], 1);

    // The 0.155 thread has no `user_message` event to take a title
    // from; the message's own `user.text` tag is what names it.
    let (title3, cli3): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT title, json(payload)->>'$.cli_version' FROM transcripts WHERE id = ?",
    )
    .bind(THREAD_3)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        title3.as_deref(),
        Some("Audit the warp core containment logs and tell me what you find.")
    );
    assert_eq!(cli3.as_deref(), Some("0.155.1"));
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM records WHERE payload->>'$.payload.type' = 'user_message'"
        )
        .await,
        4,
        "two turns in thread 1, one each in the sub-agent and thread 2; \
         the 0.155 thread carries none"
    );

    // A second pass reads nothing.
    let again = fetch(opts()).await.unwrap();
    assert_eq!((again.files, again.files_read), (4, 0));

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
        RawRange::cold(),
        16 * 1024,
    )
    .unwrap();
    assert_eq!(outcome.rendered, 4);
    assert_eq!(docs.len(), 4);

    let title_of = |d: &RenderedMarkdown| {
        d.rows
            .iter()
            .find_map(|r| r.conversation_name.clone())
            .unwrap_or_default()
    };
    let by_title = |needle: &str| {
        docs.iter()
            .find(|d| title_of(d).starts_with(needle))
            .unwrap_or_else(|| panic!("no document titled like {needle:?}"))
    };
    let t1 = by_title("The deflector dish drifts");
    let md = std::fs::read_to_string(&t1.md_path).unwrap();
    assert!(md.contains("Tool call: shell"), "{md}");
    assert!(md.contains("Tool call: apply_patch"), "{md}");
    assert!(
        md.contains("Tool result: shell"),
        "the output names the tool it answers"
    );
    assert!(md.contains("phase variance: 0.31 deg"));
    assert!(
        !md.contains("exit_code"),
        "the wrapped shell output is unwrapped, not printed as JSON"
    );
    assert!(
        md.contains("<details class=\"tool-group\""),
        "tool traffic folds"
    );
    assert!(md.contains("Lowered the realignment threshold"));
    assert!(md.contains("Checking the emitter alignment first"));
    assert!(
        !md.contains("total_token_usage"),
        "event lines are not rendered"
    );
    let kinds: std::collections::BTreeSet<&str> = t1.rows.iter().map(|r| r.kind.as_str()).collect();
    for k in [
        "User Input",
        "Harness Message",
        "LLM Thinking",
        "Tool Call",
        "Tool Result",
        "LLM Response",
    ] {
        assert!(kinds.contains(k), "missing {k} in {kinds:?}");
    }
    assert_eq!(
        t1.rows.iter().filter(|r| r.kind == "User Input").count(),
        2,
        "two typed prompts; the AGENTS.md and turn_aborted injections are harness messages"
    );
    assert!(t1
        .rows
        .iter()
        .all(|r| r.project.as_deref() == Some("enterprise")));

    let agent = by_title("Data — sub-agent of The deflector dish");
    let md = std::fs::read_to_string(&agent.md_path).unwrap();
    assert!(md.contains("Tool call: shell"));
    assert!(md.contains("aft-sensors.log"));

    // The 0.155 thread: the tags decide what the person typed, the
    // `exec` tool's JavaScript is the call, and a list of content items
    // is its output.
    let t3 = by_title("Audit the warp core containment logs");
    let md = std::fs::read_to_string(&t3.md_path).unwrap();
    assert!(md.contains("Tool call: exec"), "{md}");
    assert!(md.contains("tools.exec_command"));
    assert!(md.contains("Tool result: exec"));
    assert!(md.contains("containment 0.94"));
    assert!(md.contains("Two events crossed 0.9"));
    assert!(
        !md.contains("Reasoning"),
        "an encrypted reasoning with no summary renders nothing"
    );
    assert!(
        !md.contains("token_usage_record") && !md.contains("world_state"),
        "the newer line types are stored, not rendered"
    );
    assert_eq!(
        t3.rows.iter().filter(|r| r.kind == "User Input").count(),
        2,
        "two typed prompts — one of them opening with a tag, which only \
         `user.text` tells from an injection — and the AGENTS.md under \
         the same role is not one of them"
    );
    assert!(
        md.contains("readings look wrong to me"),
        "the prompt that looks injected is still a prompt"
    );
    assert!(
        t3.rows.iter().any(|r| r.kind == "Harness Message"),
        "and it is there as one"
    );
    assert!(t3
        .rows
        .iter()
        .any(|r| r.author.as_deref() == Some("gpt-5.6-terra")));

    let t2 = by_title("Why does the replicator return cold Earl Grey");
    let md = std::fs::read_to_string(&t2.md_path).unwrap();
    assert!(md.contains("Tool result: shell (error)"));
    assert!(md.contains("1 image(s) not shown"));
    assert!(md.contains("gpt-5.2-codex"));
    assert!(md.contains("context compacted"), "{md}");
}
