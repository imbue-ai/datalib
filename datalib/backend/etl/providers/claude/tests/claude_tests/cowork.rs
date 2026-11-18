//! A Cowork task run in the cloud, from claude.ai's sessions API to its
//! page: the TNG fixture's task downloaded by listing and by name, then
//! rendered by the conversation renderer.

use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_claude::ingest::api::{sessions_request, SESSIONS_BASE};
use datalib_etl_claude::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_claude::synthesize::ClaudeSynth;
use datalib_etl_claude_render::render::parse::parse;
use datalib_etl_claude_render::render::render::render_all;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_web::http::HttpResponse;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::{write_fixture, Synthesizer};

const ROSTER: &str = "cse_01TngRosterReadyRoom1701D";
const ORG: &str = "0aa00000-1701-4d00-8000-0000000000aa";
/// A task no org has: the fixture's org answers 404 for it, as claude.ai
/// does.
const MISSING: &str = "cse_01NoSuchTask";
/// An id claude.ai refuses outright, as it does a `cse_` of all zeros:
/// 400, with a body that names a request id of its own every time.
const REFUSED: &str = "cse_00000000000000000000000000";

async fn download(raw: &Path, configure: impl FnOnce(&mut FetchOptions)) -> FetchSummary {
    let fixture = super::claude_render::unpacked_fixture();
    let tape = tempfile::tempdir().unwrap();
    ClaudeSynth::new(fixture.path())
        .synthesize(tape.path())
        .unwrap();
    let missing = sessions_request(&format!("{SESSIONS_BASE}/{MISSING}"), ORG);
    let not_found = HttpResponse {
        status: 404,
        headers: Default::default(),
        body: br#"{"type":"error","error":{"type":"not_found_error"}}"#.to_vec(),
        duration_ms: 0,
    };
    write_fixture(tape.path(), &missing, &not_found).unwrap();
    let refused = sessions_request(&format!("{SESSIONS_BASE}/{REFUSED}"), ORG);
    let bad_request = HttpResponse {
        status: 400,
        headers: Default::default(),
        body: br#"{"error":{"message":"invalid session ID: must be a cse_... or session_... tagged ID","type":"invalid_request_error"},"request_id":"req_011TngEnterpriseD","type":"error"}"#.to_vec(),
        duration_ms: 0,
    };
    write_fixture(tape.path(), &refused, &bad_request).unwrap();
    let db = RawDb::open(&db_path_for(raw)).await.unwrap();
    let mut opts = FetchOptions {
        export_dir: Some(fixture.path().to_path_buf()),
        now: Some("2369-04-15T00:00:00Z".to_string()),
        ..FetchOptions::new(db.clone())
    };
    configure(&mut opts);
    let summary = playback::scope(tape.path(), fetch(opts)).await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    summary.expect("the download under playback")
}

/// The task renders as one conversation: the person's two prompts
/// without what the app attached to them, the picture attached to the
/// first, Claude's tool calls and their results in its replies, what it
/// said between them where it said it, and a link to the task on
/// claude.ai.
#[tokio::test(flavor = "multi_thread")]
async fn a_cowork_task_renders_as_a_conversation() {
    // Laid out as a data root is, so the picture's link is the store's
    // path under it.
    let out = tempfile::tempdir().unwrap();
    let raw = out.path().join("claude/ingest");
    std::fs::create_dir_all(&raw).unwrap();
    let summary = download(&raw, |_| {}).await;
    assert_eq!(summary.cowork_fetched, 1);

    let parsed = parse(&raw, "claude", RawRange::cold()).unwrap();
    let mut docs = Vec::new();
    render_all(
        &parsed,
        out.path(),
        "claude",
        Default::default(),
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
        1,
    )
    .unwrap();
    let doc = docs
        .iter()
        .find(|d| {
            d.rows
                .iter()
                .any(|r| r.upstream_id.as_deref() == Some(ROSTER))
        })
        .expect("the Cowork task rendered a document");
    let md = std::fs::read_to_string(&doc.md_path).unwrap();
    assert!(
        !md.contains("system-reminder"),
        "what the app attached to a prompt stays out of the page"
    );
    assert!(
        md.contains("![viewscreen.png](/data/claude/ingest/blob/"),
        "the picture the person attached is drawn from its bytes"
    );
    // What Claude said before a tool call reads before it, not folded
    // into the answer after every call.
    let said = |text: &str| md.find(text).unwrap_or_else(|| panic!("{text} in {md}"));
    assert!(said("I'll read the crew list first.") < said("Tool use: Read"));
    assert!(said("Tool result: Write") < said("Done: roster.md lists"));
    insta::assert_snapshot!("cowork_task_md", md);
    let head = doc
        .rows
        .iter()
        .find(|r| r.upstream_id.as_deref() == Some(ROSTER))
        .unwrap();
    assert_eq!(
        head.source_url.as_deref(),
        Some("https://claude.ai/cowork/cse_01TngRosterReadyRoom1701D")
    );
}

/// `cowork_session_ids` fetches the named tasks in whichever org has
/// them, by any of the spellings a person may paste, and one no org has
/// is a `config:` problem rather than a failed run.
#[tokio::test(flavor = "multi_thread")]
async fn named_tasks_are_fetched_and_a_missing_one_is_a_problem() {
    let raw = tempfile::tempdir().unwrap();
    let summary = download(raw.path(), |o| {
        o.cowork_session_ids = vec![
            "https://claude.ai/cowork/session_01TngRosterReadyRoom1701D".to_string(),
            MISSING.to_string(),
        ];
    })
    .await;
    assert_eq!(summary.cowork_fetched, 1);
    let missing: Vec<String> = summary
        .problems
        .iter()
        .map(|p| serde_json::to_value(p).unwrap().to_string())
        .collect();
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains(MISSING), "{missing:?}");
}

/// An id claude.ai refused with a 400 failed the whole Cowork phase as an
/// error, and its row quoted the response's request id, so it changed on
/// every sync. A refused id is the config's to fix, like a missing one.
#[tokio::test(flavor = "multi_thread")]
async fn an_id_claude_ai_refuses_is_a_config_problem_not_a_failed_phase() {
    let raw = tempfile::tempdir().unwrap();
    let summary = download(raw.path(), |o| {
        o.cowork_session_ids = vec![ROSTER.to_string(), REFUSED.to_string()];
    })
    .await;
    assert_eq!(summary.cowork_fetched, 1);
    assert_eq!(summary.errors, 0, "a refused id is no failed request");
    let problems: Vec<String> = summary
        .problems
        .iter()
        .map(|p| serde_json::to_value(p).unwrap().to_string())
        .collect();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("cowork_session_ids"), "{problems:?}");
    assert!(problems[0].contains(REFUSED), "{problems:?}");
    assert!(
        !problems[0].contains("req_"),
        "a request id makes the row new every run: {problems:?}"
    );
}
