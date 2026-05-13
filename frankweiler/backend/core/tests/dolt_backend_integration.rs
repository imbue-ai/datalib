//! End-to-end integration test for the Dolt backend.
//!
//! Spawns a managed `dolt sql-server` against a throwaway repo in a
//! temp directory, connects [`DoltRepo`], creates the `grid_rows` table,
//! inserts a handful of fixture rows, and verifies that
//! [`MirrorRepo::search`] returns them in the expected order — the same
//! invariants the SQLite snapshot tests check.
//!
//! Requires `dolt` on `$PATH`. The test prints a skip notice and passes
//! when `dolt` is missing so the inner `cargo test` loop stays unblocked
//! in CI environments that don't ship Dolt.

use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_core::query::parse_query;
use frankweiler_core::repo::MirrorRepo;
use frankweiler_schema::grid_rows::DDL as GRID_DDL;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

fn dolt_available() -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join("dolt").is_file() {
                return true;
            }
        }
    }
    false
}

fn pick_free_port() -> u16 {
    // Bind, read port, drop. The port may be reused briefly under
    // TIME_WAIT, but for a one-shot test that's fine — the dolt
    // server will fail loudly if it can't bind, and re-runs pick a
    // new port.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn unique_repo_dir() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fw-dolt-itest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn dolt_repo_round_trip_search_and_chat_meta() {
    if !dolt_available() {
        eprintln!("skipping: `dolt` not on $PATH");
        return;
    }

    let repo_dir = unique_repo_dir();
    let cfg = DoltConfig {
        host: "127.0.0.1".into(),
        port: pick_free_port(),
        user: "root".into(),
        repo_dirname: repo_dir.file_name().unwrap().to_string_lossy().into_owned(),
        binary: None,
    };

    let server = DoltServer::ensure(&repo_dir, &cfg).expect("dolt sql-server ready");

    // The URL the server returns uses its own derived db_name; we
    // pass the parent dir of repo_dir to DoltServer so that's
    // repo_dir.file_name(). Just use what the server reports.
    let url = server.mysql_url();

    let root = Arc::new(repo_dir.parent().unwrap().to_path_buf());
    let repo = DoltRepo::connect(&url, root)
        .await
        .unwrap_or_else(|e| panic!("connect dolt at {url}: {e}"));

    // The Dolt working set is per-session (per-connection). With
    // sqlx's connection pool, a SELECT from a different connection
    // than the INSERT won't see uncommitted rows — so we pin to one
    // connection for setup and run `CALL DOLT_COMMIT(...)` to publish
    // changes once. This mirrors what production writes will do via
    // T12's `insert_feedback` + DOLT_COMMIT pair.
    let mut conn = repo.pool().acquire().await.expect("acquire pool conn");
    for (_t, ddl) in GRID_DDL {
        sqlx::query(ddl)
            .execute(&mut *conn)
            .await
            .expect("create grid_rows");
    }
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link, qmd_path, source_url) \
         VALUES ('c-1','anthropic','Chat','Claude','2026-04-01T10:00:00+00:00', \
                 NULL,'acct-a',NULL,NULL,'Test conv','c-1',NULL,'/chat/c-1', \
                 'summary','', 'chats/c-1.md', 'https://claude.ai/chat/c-1')",
    )
    .execute(&mut *conn)
    .await
    .expect("insert chat row");
    sqlx::query(
        "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
         author, account, project, channel, conversation_name, conversation_uuid, \
         message_index, entire_chat, text, slack_link) \
         VALUES ('m-1','anthropic','User Input','Claude','2026-04-01T10:01:00+00:00', \
                 'acct-a','acct-a',NULL,NULL,'Test conv','c-1',0,'/chat/c-1','hello there','')",
    )
    .execute(&mut *conn)
    .await
    .expect("insert message row");
    sqlx::query("CALL DOLT_COMMIT('-Am', 'test fixture setup')")
        .execute(&mut *conn)
        .await
        .expect("dolt commit");
    drop(conn);

    let rows = repo.search(&parse_query(""), 100).await.unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows, got {rows:?}");
    // Chat tiebreaks before its message.
    assert_eq!(rows[0].kind, "Chat");
    assert_eq!(rows[1].kind, "User Input");

    let filtered = repo
        .search(&parse_query("source:Claude type:all"), 100)
        .await
        .unwrap();
    assert!(!filtered.is_empty());
    assert!(filtered.iter().all(|r| r.source == "Claude"));

    let meta = repo
        .chat_meta("c-1")
        .await
        .unwrap()
        .expect("chat meta present");
    assert_eq!(meta.name.as_deref(), Some("Test conv"));
    assert_eq!(meta.source_label.as_deref(), Some("Claude"));
    assert_eq!(
        meta.source_url.as_deref(),
        Some("https://claude.ai/chat/c-1")
    );

    let qmd = repo.qmd_path_for_conversation("c-1").await.unwrap();
    // Should be absolute, joined with the root we passed in.
    assert!(qmd.is_some());
    let qmd = qmd.unwrap();
    assert!(qmd.is_absolute(), "expected absolute qmd path, got {qmd:?}");
    assert!(qmd.to_string_lossy().ends_with("chats/c-1.md"));

    // Drop the server explicitly to kill the subprocess before we
    // clean up the temp dir — otherwise the dolt server keeps the
    // dir locked.
    drop(repo);
    drop(server);
    let _ = std::fs::remove_dir_all(&repo_dir);
}
