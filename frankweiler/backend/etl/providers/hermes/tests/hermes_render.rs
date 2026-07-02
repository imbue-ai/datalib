//! Render golden for the Hermes provider against the synthetic export fixture.
//!
//! Proves that Hermes/OpenClaw export files (JSONL session export, JSON
//! snapshot, and generic OpenClaw-shaped records) parse into conversations and
//! render to Markdown + grid_rows.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use frankweiler_etl_hermes::local::import_local;
use frankweiler_etl_hermes::render_and_index_md::parse::parse_export_dir;
use frankweiler_etl_hermes::render_and_index_md::render::render_all;
use frankweiler_etl_hermes_config::HermesSync;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HERMES_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes_export")
}

fn collect_by_ext(root: &std::path::Path, ext: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        ext: &str,
        out: &mut BTreeMap<String, String>,
    ) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, ext, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                if rel.ends_with(ext) {
                    out.insert(rel, fs::read_to_string(&p).unwrap());
                }
            }
        }
    }
    walk(root, root, ext, &mut out);
    out
}

async fn write_synthetic_state_db(root: &std::path::Path) {
    fs::create_dir_all(root).expect("mkdir hermes root");
    let db_path = root.join("state.db");
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open synthetic state.db");
    for stmt in [
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT, user_id TEXT, model TEXT,
            parent_session_id TEXT, started_at REAL, title TEXT)",
        "CREATE TABLE messages (
            id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, tool_name TEXT,
            tool_calls TEXT, reasoning TEXT, reasoning_content TEXT,
            model TEXT, timestamp REAL, active INTEGER)",
        "INSERT INTO sessions VALUES
            ('sync-session','telegram','local-user','gpt-local',NULL,1790000000.0,'Local sync demo')",
        "INSERT INTO messages VALUES
            (1,'sync-session','user','hello from local sync',NULL,NULL,NULL,NULL,NULL,1790000001.0,1)",
        "INSERT INTO messages VALUES
            (2,'sync-session','assistant','the local answer',NULL,NULL,'reasoned locally',NULL,'gpt-local',1790000002.0,1)",
        "INSERT INTO messages VALUES
            (3,'sync-session','tool','stdout from local tool','shell','[{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}]',NULL,NULL,NULL,1790000003.0,1)",
        "INSERT INTO messages VALUES
            (4,'sync-session','assistant','rewound and should not render',NULL,NULL,NULL,NULL,NULL,1790000004.0,0)",
    ] {
        sqlx::query(stmt).execute(&pool).await.expect("sqlite stmt");
    }
    pool.close().await;
}

#[test]
fn renders_hermes_fixture() {
    let parsed = parse_export_dir(&fixture_dir()).expect("parse");
    // Three sessions: CLI chat, Telegram agent trace, OpenClaw-generic.
    assert_eq!(parsed.sessions.len(), 3, "expected 3 sessions");

    let tmp = tempfile::tempdir().expect("tmp");
    let priors = std::collections::HashMap::new();
    render_all(
        &parsed,
        tmp.path(),
        "hermes",
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
        &mut |_doc| Ok(()),
    )
    .expect("render");

    let md = collect_by_ext(tmp.path(), ".md");
    let mut bundle = String::new();
    for (path, body) in &md {
        bundle.push_str("=== ");
        bundle.push_str(path);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    insta::assert_snapshot!("hermes_md_tree", bundle);

    let sidecars = collect_by_ext(tmp.path(), ".grid_rows.json");
    let mut sbundle = String::new();
    for (path, body) in &sidecars {
        sbundle.push_str("=== ");
        sbundle.push_str(path);
        sbundle.push_str(" ===\n");
        sbundle.push_str(body);
        sbundle.push('\n');
    }
    insta::assert_snapshot!("hermes_sidecar_tree", sbundle);
}

#[tokio::test]
async fn managed_local_sync_renders_synthetic_state_db() {
    let source = tempfile::tempdir().expect("source tempdir");
    let hermes_root = source.path().join(".hermes");
    write_synthetic_state_db(&hermes_root).await;

    let sync = HermesSync {
        roots: vec![hermes_root],
        ..Default::default()
    };
    let (parsed, stats) = import_local(&sync).await.expect("local import");
    assert_eq!(stats.roots, 1);
    assert_eq!(stats.dbs, 1);
    assert_eq!(stats.sessions, 1);

    let out = tempfile::tempdir().expect("output tempdir");
    let priors = std::collections::HashMap::new();
    render_all(
        &parsed,
        out.path(),
        "hermes-local",
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
        &mut |_doc| Ok(()),
    )
    .expect("render local sync");

    let md = collect_by_ext(out.path(), ".md");
    assert_eq!(md.len(), 1, "one rendered conversation");
    let body = md.values().next().expect("markdown body");
    assert!(body.contains("Local sync demo"), "body: {body}");
    assert!(body.contains("hello from local sync"), "body: {body}");
    assert!(body.contains("the local answer"), "body: {body}");
    assert!(body.contains("> reasoned locally"), "body: {body}");
    assert!(body.contains("stdout from local tool"), "body: {body}");
    assert!(
        !body.contains("rewound and should not render"),
        "body: {body}"
    );

    let sidecars = collect_by_ext(out.path(), ".grid_rows.json");
    assert_eq!(sidecars.len(), 1, "one grid sidecar");
    let sidecar = sidecars.values().next().expect("sidecar body");
    assert!(
        sidecar.contains(r#"provider": "hermes"#),
        "sidecar: {sidecar}"
    );
    assert!(
        sidecar.contains(r#"project": "telegram"#),
        "sidecar: {sidecar}"
    );
    assert!(
        sidecar.contains(r#"account": "local-user"#),
        "sidecar: {sidecar}"
    );
    assert!(
        sidecar.contains(r#"kind": "Tool Call"#),
        "sidecar: {sidecar}"
    );
}
