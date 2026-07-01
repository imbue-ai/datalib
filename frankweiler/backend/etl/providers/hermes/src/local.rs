//! Managed local import for the `hermes` source: discover the Hermes /
//! OpenClaw agent history already on this machine and parse it into the same
//! [`ParsedHermesExport`] the export-directory path produces.
//!
//! This is the primary UX (`sync: {}`), analogous to the `chatgpt_api` /
//! `anthropic` managed sync modes: the user doesn't have to export anything
//! first. We look in the well-known local roots, read each root's `state.db`
//! read-only (the source DB file itself is never copied and never mutated), and
//! additionally fold in any legacy `<root>/sessions/*.json` files.
//!
//! Privacy note: "not copied" refers only to the source DB *file*. The
//! transcript *contents* it holds are read and mirrored into datalib's
//! `data_root` as rendered Markdown plus grid/index rows — so the output is
//! just as sensitive as the source (it can contain system prompts, memory, and
//! tool output). Treat the datalib data_root accordingly.
//!
//! Default discovery treats absent well-known roots as "nothing to import from
//! here" rather than an error. Explicit `sync.roots` are different: if a user
//! names a root, a missing path is most likely a typo and fails loudly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use frankweiler_etl_hermes_config::HermesSync;

use crate::render_and_index_md::parse::{
    parse_export_dir, parse_jsonl_files, HermesMessage, HermesSession, ParsedHermesExport,
};

/// Summary of what local discovery found, surfaced in the processor's run
/// message.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscoveryStats {
    /// Roots that existed and were scanned.
    pub roots: usize,
    /// `state.db` files read.
    pub dbs: usize,
    /// Legacy `sessions/*.json` directories folded in.
    pub legacy_dirs: usize,
    /// OpenClaw `agents/*/sessions/*.jsonl` event-log files folded in.
    pub openclaw_files: usize,
    /// Sessions parsed across all sources.
    pub sessions: usize,
}

/// Discover local roots, read their `state.db` + legacy JSON, and merge
/// everything into one [`ParsedHermesExport`]. Async because SQLite reads go
/// through `sqlx`.
pub async fn import_local(sync: &HermesSync) -> Result<(ParsedHermesExport, DiscoveryStats)> {
    // Explicit `sync.roots` are a user assertion that these paths exist; a typo
    // that silently expands to nothing would import an empty transcript and look
    // like success. Fail loudly instead. (Default discovery still ignores absent
    // optional roots — see `discover_roots`.)
    if !sync.roots.is_empty() {
        let home = home_dir();
        let missing: Vec<PathBuf> = sync
            .roots
            .iter()
            .map(|r| expand_tilde(r, home.as_deref()))
            .filter(|p| !p.exists())
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(
                "hermes: configured `sync.roots` do not exist: {list} \
                 (check for typos; default discovery is used only when `roots` is empty)"
            ));
        }
    }

    let roots = discover_roots(sync);
    let mut stats = DiscoveryStats::default();
    let mut merged = ParsedHermesExport::default();
    // Deterministic dedupe: the first occurrence of a session id wins; later
    // duplicates are skipped with a tracing warning. `state.db` is read before
    // legacy JSON within a root, so within a root the DB copy wins over a legacy
    // export of the same session. Discovery visits roots in a fixed order, so
    // the winner is stable across runs.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for root in &roots {
        stats.roots += 1;

        // 1. state.db (canonical Hermes store), read-only.
        let db_path = root.join("state.db");
        if db_path.is_file() {
            match read_state_db(&db_path).await {
                Ok(sessions) => {
                    stats.dbs += 1;
                    extend_deduped(&mut merged, &mut seen, sessions, "state.db");
                }
                Err(err) => {
                    tracing::warn!(db = %db_path.display(), error = %err, "hermes: failed to read state.db; skipping");
                }
            }
        }

        // 2. Legacy JSON sessions under <root>/sessions/*.json.
        if sync.include_legacy_json_sessions() {
            let sessions_dir = root.join("sessions");
            if sessions_dir.is_dir() {
                let parsed = parse_export_dir(&sessions_dir).with_context(|| {
                    format!("hermes legacy sessions {}", sessions_dir.display())
                })?;
                if !parsed.sessions.is_empty() {
                    stats.legacy_dirs += 1;
                    extend_deduped(&mut merged, &mut seen, parsed.sessions, "legacy json");
                }
            }
        }

        // 3. OpenClaw agent event logs under <root>/agents/*/sessions/*.jsonl.
        //    Parsed file-by-file (not via parse_export_dir) so the sibling
        //    `sessions.json` metadata index is left out of the transcript.
        let openclaw_jsonl = collect_openclaw_agent_jsonl(root);
        if !openclaw_jsonl.is_empty() {
            match parse_jsonl_files(&openclaw_jsonl) {
                Ok(parsed) => {
                    if !parsed.sessions.is_empty() {
                        stats.openclaw_files += openclaw_jsonl.len();
                        extend_deduped(&mut merged, &mut seen, parsed.sessions, "openclaw jsonl");
                    }
                }
                Err(err) => {
                    tracing::warn!(root = %root.display(), error = %err, "hermes: failed to parse OpenClaw agent jsonl; skipping");
                }
            }
        }
    }

    stats.sessions = merged.sessions.len();
    Ok((merged, stats))
}

/// Append `sessions` into `merged`, skipping any whose id was already seen
/// (first-wins), emitting a tracing warning per skipped duplicate. `origin`
/// labels the source of the incoming batch for the warning.
fn extend_deduped(
    merged: &mut ParsedHermesExport,
    seen: &mut std::collections::HashSet<String>,
    sessions: Vec<HermesSession>,
    origin: &str,
) {
    for session in sessions {
        if seen.insert(session.id.clone()) {
            merged.sessions.push(session);
        } else {
            tracing::warn!(
                session_id = %session.id,
                origin,
                "hermes: duplicate session id; keeping the first occurrence and skipping this one"
            );
        }
    }
}

/// Resolve the set of roots to scan.
///
/// * `sync.roots` non-empty → exactly those (tilde-expanded). Missing explicit
///   roots are rejected by [`import_local`] before scanning.
/// * Otherwise default discovery: `$HOME/.hermes` (+ `profiles/*` when
///   `include_profiles`), plus OpenClaw-compatible roots when present.
pub fn discover_roots(sync: &HermesSync) -> Vec<PathBuf> {
    let home = home_dir();

    let mut roots: Vec<PathBuf> = Vec::new();
    if !sync.roots.is_empty() {
        for r in &sync.roots {
            roots.push(expand_tilde(r, home.as_deref()));
        }
    } else if let Some(home) = home.as_deref() {
        let hermes = home.join(".hermes");
        push_if_exists(&mut roots, hermes.clone());
        if sync.include_profiles() {
            push_profile_dirs(&mut roots, &hermes.join("profiles"));
        }
        // OpenClaw-compatible roots — additive, never required.
        push_if_exists(&mut roots, home.join(".openclaw"));
        push_if_exists(
            &mut roots,
            home.join("Library/Application Support/OpenClaw"),
        );
    }

    // De-dupe while preserving order, and keep only roots that exist.
    let mut seen = std::collections::HashSet::new();
    roots
        .into_iter()
        .filter(|p| p.exists())
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Collect `<root>/agents/*/sessions/*.jsonl` event logs (OpenClaw layout), in
/// deterministic sorted order. Returns empty if there's no `agents/` dir.
fn collect_openclaw_agent_jsonl(root: &Path) -> Vec<PathBuf> {
    let Ok(agents) = std::fs::read_dir(root.join("agents")) else {
        return Vec::new();
    };
    let mut agent_dirs: Vec<PathBuf> = agents
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    agent_dirs.sort();

    let mut files: Vec<PathBuf> = Vec::new();
    for agent in agent_dirs {
        let Ok(entries) = std::fs::read_dir(agent.join("sessions")) else {
            continue;
        };
        let mut jsonl: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("jsonl"))
                    .unwrap_or(false)
            })
            .collect();
        jsonl.sort();
        files.extend(jsonl);
    }
    files
}

fn push_if_exists(out: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() {
        out.push(path);
    }
}

/// Add each immediate subdirectory of `profiles_dir` (e.g.
/// `~/.hermes/profiles/work`). Sorted for determinism.
fn push_profile_dirs(out: &mut Vec<PathBuf>, profiles_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(profiles_dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    out.extend(dirs);
}

/// Read `sessions` joined to `messages` from a Hermes `state.db`, read-only,
/// mapping rows into the shared [`HermesSession`] / [`HermesMessage`] structs.
///
/// Defensive about the schema: optional columns (`reasoning`, `tool_calls`,
/// `active`, …) are read only when the table actually has them, so a slightly
/// older or newer store still imports.
async fn read_state_db(db_path: &Path) -> Result<Vec<HermesSession>> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await
        .with_context(|| format!("open hermes state.db {}", db_path.display()))?;

    let msg_cols = table_columns(&pool, "messages").await?;
    let sess_cols = table_columns(&pool, "sessions").await?;
    let has = |cols: &[String], name: &str| cols.iter().any(|c| c == name);

    // Build the message projection defensively; alias every optional column so
    // row access is uniform regardless of what the store actually carries.
    let reasoning_expr = match (
        has(&msg_cols, "reasoning"),
        has(&msg_cols, "reasoning_content"),
    ) {
        (true, true) => "COALESCE(m.reasoning, m.reasoning_content)",
        (true, false) => "m.reasoning",
        (false, true) => "m.reasoning_content",
        (false, false) => "NULL",
    };
    let tool_name_expr = if has(&msg_cols, "tool_name") {
        "m.tool_name"
    } else {
        "NULL"
    };
    let tool_calls_expr = if has(&msg_cols, "tool_calls") {
        "m.tool_calls"
    } else {
        "NULL"
    };
    let msg_model_expr = if has(&msg_cols, "model") {
        "m.model"
    } else {
        "NULL"
    };
    // Stable upstream message id, spelled `id`, `message_id`, or `uuid`
    // depending on the store variant. First present wins.
    let msg_id_expr = if has(&msg_cols, "id") {
        "m.id"
    } else if has(&msg_cols, "message_id") {
        "m.message_id"
    } else if has(&msg_cols, "uuid") {
        "m.uuid"
    } else {
        "NULL"
    };
    let ts_expr = if has(&msg_cols, "timestamp") {
        "m.timestamp"
    } else {
        "NULL"
    };
    let active_filter = if has(&msg_cols, "active") {
        "WHERE m.active IS NULL OR m.active <> 0"
    } else {
        ""
    };
    // Order by timestamp when present, else leave insertion order (rowid).
    let order = if has(&msg_cols, "timestamp") {
        "ORDER BY m.session_id, m.timestamp, m.rowid"
    } else {
        "ORDER BY m.session_id, m.rowid"
    };

    // Session-level optional columns.
    let sess_source = if has(&sess_cols, "source") {
        "s.source"
    } else {
        "NULL"
    };
    let sess_user = if has(&sess_cols, "user_id") {
        "s.user_id"
    } else {
        "NULL"
    };
    let sess_model = if has(&sess_cols, "model") {
        "s.model"
    } else {
        "NULL"
    };
    let sess_parent = if has(&sess_cols, "parent_session_id") {
        "s.parent_session_id"
    } else {
        "NULL"
    };
    let sess_started = if has(&sess_cols, "started_at") {
        "s.started_at"
    } else {
        "NULL"
    };
    let sess_title = if has(&sess_cols, "title") {
        "s.title"
    } else {
        "NULL"
    };

    // One query, sessions LEFT JOIN messages, so sessions with no (active)
    // messages still surface their metadata.
    let sql = format!(
        "SELECT \
            s.id AS session_id, \
            {sess_source} AS s_source, \
            {sess_user} AS s_user_id, \
            {sess_model} AS s_model, \
            {sess_parent} AS s_parent, \
            {sess_started} AS s_started_at, \
            {sess_title} AS s_title, \
            {msg_id_expr} AS m_id, \
            m.role AS m_role, \
            m.content AS m_content, \
            {reasoning_expr} AS m_reasoning, \
            {tool_name_expr} AS m_tool_name, \
            {tool_calls_expr} AS m_tool_calls, \
            {msg_model_expr} AS m_model, \
            {ts_expr} AS m_timestamp \
         FROM sessions s \
         LEFT JOIN messages m ON m.session_id = s.id {active_filter} \
         {order}",
    );

    let rows = sqlx::query(&sql)
        .fetch_all(&pool)
        .await
        .with_context(|| format!("query hermes sessions/messages in {}", db_path.display()))?;

    // Accumulate in first-appearance session order.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, HermesSession> =
        std::collections::HashMap::new();

    for row in &rows {
        let sid: String = row.try_get("session_id").unwrap_or_default();
        if sid.is_empty() {
            continue;
        }
        let session = by_id.entry(sid.clone()).or_insert_with(|| {
            order.push(sid.clone());
            HermesSession {
                id: sid.clone(),
                title: try_str(row, "s_title"),
                source: try_str(row, "s_source"),
                model: try_str(row, "s_model"),
                user_id: try_str(row, "s_user_id"),
                parent_session_id: try_str(row, "s_parent"),
                started_at_ms: try_ms(row, "s_started_at"),
                messages: Vec::new(),
            }
        });

        // A LEFT JOIN with no messages yields a row whose message fields are all
        // NULL — skip those (metadata already captured above).
        let role = try_str(row, "m_role");
        let content = try_str(row, "m_content");
        let reasoning = try_str(row, "m_reasoning");
        let tool_name = try_str(row, "m_tool_name");
        let tool_calls = try_str(row, "m_tool_calls");
        if role.is_none()
            && content.is_none()
            && reasoning.is_none()
            && tool_name.is_none()
            && tool_calls.is_none()
        {
            continue;
        }

        session.messages.push(HermesMessage {
            id: try_str(row, "m_id"),
            role: role.unwrap_or_else(|| "assistant".to_string()),
            content,
            reasoning,
            tool_name,
            tool_calls_pretty: tool_calls.and_then(|s| pretty_json_str(&s)),
            model: try_str(row, "m_model"),
            timestamp_ms: try_ms(row, "m_timestamp"),
        });
    }

    pool.close().await;

    Ok(order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}

/// PRAGMA table_info → column names. Empty vec if the table doesn't exist.
async fn table_columns(pool: &sqlx::SqlitePool, table: &str) -> Result<Vec<String>> {
    // `table` is a fixed identifier from this module, not user input.
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .with_context(|| format!("pragma table_info({table})"))?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<String, _>("name").ok())
        .collect())
}

/// Read a column as an owned `String`, tolerating NULL and non-text affinities.
/// Every column is aliased in the SELECT, so it always exists in the result set.
fn try_str(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(col)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<i64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v.to_string())
        })
        .or_else(|| {
            row.try_get::<Option<f64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v.to_string())
        })
        .filter(|s| !s.is_empty())
}

/// Read a timestamp column that may be an INTEGER/REAL (epoch secs or ms) or a
/// TEXT (numeric or RFC3339) and coerce to unix milliseconds.
fn try_ms(row: &sqlx::sqlite::SqliteRow, col: &str) -> Option<i64> {
    if let Ok(Some(f)) = row.try_get::<Option<f64>, _>(col) {
        return Some(secs_or_ms_to_ms(f));
    }
    if let Ok(Some(i)) = row.try_get::<Option<i64>, _>(col) {
        return Some(secs_or_ms_to_ms(i as f64));
    }
    let s = row.try_get::<Option<String>, _>(col).ok().flatten()?;
    chrono::DateTime::parse_from_rfc3339(&s)
        .ok()
        .map(|d| d.timestamp_millis())
        .or_else(|| s.parse::<f64>().ok().map(secs_or_ms_to_ms))
}

fn secs_or_ms_to_ms(f: f64) -> i64 {
    if f.abs() >= 1e12 {
        f as i64
    } else {
        (f * 1000.0) as i64
    }
}

/// Pretty-print a `tool_calls` TEXT value (Hermes stores JSON as text).
fn pretty_json_str(s: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    if value.is_null() {
        return None;
    }
    serde_json::to_string_pretty(&value).ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Expand a leading `~` / `~/` against `home`.
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let (Some(home), Some(s)) = (home, path.to_str()) else {
        return path.to_path_buf();
    };
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_against_home() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            expand_tilde(Path::new("~/.hermes"), Some(&home)),
            PathBuf::from("/home/u/.hermes")
        );
        assert_eq!(expand_tilde(Path::new("~"), Some(&home)), home);
        assert_eq!(
            expand_tilde(Path::new("/abs/path"), Some(&home)),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn dedupe_keeps_first_session_id() {
        let mut merged = ParsedHermesExport::default();
        let mut seen = std::collections::HashSet::new();
        let mk = |id: &str, title: &str| HermesSession {
            id: id.to_string(),
            title: Some(title.to_string()),
            ..Default::default()
        };
        // Simulate DB read first, then legacy JSON with an overlapping id.
        extend_deduped(
            &mut merged,
            &mut seen,
            vec![mk("a", "db-a"), mk("b", "db-b")],
            "state.db",
        );
        extend_deduped(
            &mut merged,
            &mut seen,
            vec![mk("b", "legacy-b"), mk("c", "legacy-c")],
            "legacy json",
        );
        let ids: Vec<&str> = merged.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // The DB copy of "b" won (first occurrence), not the legacy one.
        let b = merged.sessions.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.title.as_deref(), Some("db-b"));
    }

    #[tokio::test]
    async fn explicit_missing_root_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let sync = HermesSync {
            roots: vec![missing],
            ..Default::default()
        };
        let err = import_local(&sync).await.unwrap_err();
        assert!(err.to_string().contains("do not exist"), "got: {err}");
    }

    #[test]
    fn ms_coercion() {
        assert_eq!(secs_or_ms_to_ms(1_790_000_001.0), 1_790_000_001_000);
        assert_eq!(secs_or_ms_to_ms(1_790_000_001_000.0), 1_790_000_001_000);
    }

    /// OpenClaw agent event logs under `agents/*/sessions/*.jsonl` are folded
    /// in, while a sibling `sessions.json` metadata index is left out of the
    /// transcript. Synthetic fixtures only, explicit root (no HOME dependency).
    #[tokio::test]
    async fn local_import_reads_openclaw_agent_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".openclaw");
        let sessions_dir = root.join("agents/main/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // A metadata index that must NOT become a transcript session.
        std::fs::write(
            sessions_dir.join("sessions.json"),
            r#"{"sessions":[{"id":"test","title":"idx"}]}"#,
        )
        .unwrap();

        // The actual event log.
        let events = [
            r#"{"type":"session","version":3,"id":"test","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"model_change","id":"mc","provider":"openrouter","modelId":"anthropic/claude"}"#,
            r#"{"type":"message","id":"66bdd77e","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":1769849224436}}"#,
            r#"{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi back"}],"timestamp":1769849225000}}"#,
        ]
        .join("\n");
        std::fs::write(sessions_dir.join("test.jsonl"), events).unwrap();

        let sync = HermesSync {
            roots: vec![root.clone()],
            ..Default::default()
        };
        let (parsed, stats) = import_local(&sync).await.unwrap();

        assert_eq!(stats.roots, 1);
        assert_eq!(stats.openclaw_files, 1);
        // Only the event-log session, not a "test"/idx snapshot from sessions.json.
        assert_eq!(parsed.sessions.len(), 1);
        let s = &parsed.sessions[0];
        assert_eq!(s.id, "test");
        assert_eq!(s.source.as_deref(), Some("openclaw"));
        assert_eq!(s.model.as_deref(), Some("openrouter/anthropic/claude"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].reasoning.as_deref(), Some("hmm"));
        assert_eq!(s.messages[1].content.as_deref(), Some("hi back"));
    }

    /// Write a minimal synthetic Hermes `state.db` covering the columns the
    /// reader queries, then import it via explicit `sync.roots` (no HOME
    /// dependency, no private fixtures) and assert a session with a tool
    /// result + reasoning is parsed.
    #[tokio::test]
    async fn local_import_reads_synthetic_state_db() {
        use sqlx::sqlite::SqlitePoolOptions;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".hermes");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("state.db");

        // Create the DB with the sessions/messages subset our reader expects.
        {
            let opts = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            for stmt in [
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, source TEXT, user_id TEXT, model TEXT,
                    parent_session_id TEXT, started_at REAL, title TEXT)",
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, tool_name TEXT,
                    tool_calls TEXT, reasoning TEXT, reasoning_content TEXT,
                    model TEXT, timestamp REAL, active INTEGER)",
                "INSERT INTO sessions VALUES
                    ('s1','cli','u1','gpt-x',NULL,1790000000.0,'Demo session')",
                "INSERT INTO messages VALUES
                    (1,'s1','user','hello',NULL,NULL,NULL,NULL,NULL,1790000001.0,1)",
                "INSERT INTO messages VALUES
                    (2,'s1','assistant','the answer',NULL,NULL,'thinking hard',NULL,'gpt-x',1790000002.0,1)",
                "INSERT INTO messages VALUES
                    (3,'s1','tool','stdout here','shell','[{\"name\":\"shell\"}]',NULL,NULL,NULL,1790000003.0,1)",
                // Inactive message must be filtered out.
                "INSERT INTO messages VALUES
                    (4,'s1','assistant','rewound',NULL,NULL,NULL,NULL,NULL,1790000004.0,0)",
                // reasoning_content fallback (COALESCE) on another session.
                "INSERT INTO sessions VALUES
                    ('s2','telegram','u1','gpt-x',NULL,1790000010.0,'Second')",
                "INSERT INTO messages VALUES
                    (5,'s2','assistant','ok',NULL,NULL,NULL,'fallback reasoning','gpt-x',1790000011.0,1)",
            ] {
                sqlx::query(stmt).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }

        let sync = HermesSync {
            roots: vec![root.clone()],
            ..Default::default()
        };
        let (parsed, stats) = import_local(&sync).await.unwrap();

        assert_eq!(stats.roots, 1);
        assert_eq!(stats.dbs, 1);
        assert_eq!(parsed.sessions.len(), 2);

        let s1 = &parsed.sessions[0];
        assert_eq!(s1.id, "s1");
        assert_eq!(s1.source.as_deref(), Some("cli"));
        assert_eq!(s1.title.as_deref(), Some("Demo session"));
        // user + assistant + tool (rewound dropped).
        assert_eq!(s1.messages.len(), 3);
        assert_eq!(s1.messages[0].id.as_deref(), Some("1"));
        let assistant = &s1.messages[1];
        assert_eq!(assistant.reasoning.as_deref(), Some("thinking hard"));
        let tool = &s1.messages[2];
        assert_eq!(tool.tool_name.as_deref(), Some("shell"));
        assert!(tool.tool_calls_pretty.as_deref().unwrap().contains("shell"));

        // COALESCE(reasoning, reasoning_content) picks the fallback column.
        let s2 = &parsed.sessions[1];
        assert_eq!(
            s2.messages[0].reasoning.as_deref(),
            Some("fallback reasoning")
        );
    }
}
