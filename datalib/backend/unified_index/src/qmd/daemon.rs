//! Long-lived `qmd mcp` subprocess.

use crate::qmd::mapping::{CollectionScope, QmdHit, QueryMode};
use crate::qmd::runner::{has_lex_syntax, strip_lex_syntax, strip_uri, DEFAULT_QMD_VERSION};
use crate::qmd::{qmd_cache_home, qmd_index_path};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct QmdDaemonConfig {
    pub qmd_root: PathBuf,
    pub qmd_version: String,
}

impl QmdDaemonConfig {
    pub fn new(qmd_root: impl Into<PathBuf>) -> Self {
        Self {
            qmd_root: qmd_root.into(),
            qmd_version: DEFAULT_QMD_VERSION.into(),
        }
    }
}

pub struct QmdDaemon {
    cfg: QmdDaemonConfig,
    state: Mutex<DaemonState>,
}

struct DaemonState {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    next_id: u64,
    /// mtime of the index the live child was spawned against. When the
    /// index on disk is newer (a sync rebuilt it), the child holds a
    /// stale view and must be respawned. `None` when no child is live.
    index_mtime: Option<SystemTime>,
}

impl QmdDaemon {
    /// Build a daemon handle. Neither the child nor the index is touched
    /// here: the index may not exist yet (empty data root, first sync
    /// still pending) and may be rebuilt while the app runs. Both are
    /// resolved lazily per [`search`] — a daemon built against a missing
    /// index starts serving the moment a sync creates it, and picks up a
    /// rebuilt index on the next query (see [`ensure_started`]). So the
    /// child isn't spawned until the first search either, keeping startup
    /// cheap when nobody is searching.
    pub fn new(cfg: QmdDaemonConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(DaemonState {
                child: None,
                stdin: None,
                stdout: None,
                next_id: 0,
                index_mtime: None,
            }),
        }
    }

    pub fn config(&self) -> &QmdDaemonConfig {
        &self.cfg
    }

    /// Run a search. On any I/O error the child is torn down so the next
    /// call respawns cleanly; the caller decides whether to fall back to
    /// the CLI path.
    pub fn search(
        &self,
        mode: QueryMode,
        q: &str,
        limit: usize,
        scope: &CollectionScope,
    ) -> Result<Vec<QmdHit>> {
        // An empty scope matches nothing. qmd would read an empty
        // `collections` array as "unscoped" and answer with everything,
        // so this case never reaches it.
        if scope.is_empty() {
            return Ok(Vec::new());
        }
        // The MCP `query` tool requires typed sub-queries — there's no
        // bare auto-expand entry point like the CLI's `qmd query "<text>"`.
        // For Hybrid we send lex+vec (qmd's own "best recall" recipe);
        // for Vsearch we send vec only. First sub-query gets 2× weight,
        // so lex goes first when present (better behavior on exact terms
        // like UUIDs, channel names, usernames).
        let searches = build_daemon_searches(mode, q);
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("daemon mutex poisoned"))?;
        let res = (|| -> Result<Vec<QmdHit>> {
            // The index may be absent (no sync yet) or freshly rebuilt.
            // Its mtime both gates the search and tells `ensure_started`
            // whether the live child is stale. A missing index is a
            // normal fallback signal, not a daemon failure.
            let idx = qmd_index_path(&self.cfg.qmd_root);
            let index_mtime = std::fs::metadata(&idx)
                .and_then(|m| m.modified())
                .map_err(|_| {
                    anyhow!(
                        "qmd index not found at {} — sync to build it",
                        idx.display()
                    )
                })?;
            ensure_started(&mut guard, &self.cfg, index_mtime)?;
            guard.next_id = guard.next_id.wrapping_add(1);
            let id = guard.next_id;
            let mut arguments = serde_json::json!({
                "searches": searches,
                "limit": limit,
                "rerank": false,
            });
            // Scoping happens *inside* retrieval: qmd searches each named
            // collection and merges, so a source's hits cannot be crowded
            // out of a global top-N by a larger one. Filtering the results
            // afterwards — what the applet used to do alone — returns
            // nothing at all whenever that crowding happens.
            if let Some(names) = scope.names() {
                arguments["collections"] = serde_json::json!(names);
            }
            let req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": "query", "arguments": arguments },
            });
            send_request(&mut guard, &req)?;
            let resp = read_response(&mut guard, id)?;
            parse_query_response(&resp)
        })();
        if res.is_err() {
            teardown(&mut guard);
        }
        res
    }
}

/// Build the MCP `searches` JSON array for a given user query and mode.
/// Extracted so the lex/vec routing is unit-testable without spawning
/// the daemon. See [`crate::qmd::runner::has_lex_syntax`] /
/// [`crate::qmd::runner::strip_lex_syntax`] for the shared rules used
/// by the CLI fallback path.
fn build_daemon_searches(mode: QueryMode, q: &str) -> serde_json::Value {
    let lex_has_syntax = has_lex_syntax(q);
    let vec_text = if lex_has_syntax {
        strip_lex_syntax(q)
    } else {
        q.to_string()
    };
    match mode {
        QueryMode::Hybrid => {
            let mut subs = vec![serde_json::json!({"type": "lex", "query": q})];
            if !vec_text.is_empty() {
                subs.push(serde_json::json!({"type": "vec", "query": vec_text}));
            }
            serde_json::Value::Array(subs)
        }
        QueryMode::Vsearch => serde_json::json!([
            {"type": "vec", "query": if vec_text.is_empty() { q } else { vec_text.as_str() }},
        ]),
    }
}

fn ensure_started(
    state: &mut DaemonState,
    cfg: &QmdDaemonConfig,
    index_mtime: SystemTime,
) -> Result<()> {
    // A rebuilt index (mtime moved) means the resident child opened a
    // now-stale copy — tear it down so we respawn against the fresh
    // file. `teardown` clears `index_mtime`, so the checks below fall
    // through to a clean spawn.
    if state.index_mtime != Some(index_mtime) {
        teardown(state);
    }
    // If we still have a child, make sure it's alive — `try_wait`
    // returns `Some(_)` if the process exited.
    if let Some(child) = state.child.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => teardown(state),
            Ok(None) => return Ok(()),
            Err(_) => teardown(state),
        }
    }
    spawn(state, cfg, index_mtime)
}

fn spawn(state: &mut DaemonState, cfg: &QmdDaemonConfig, index_mtime: SystemTime) -> Result<()> {
    let mut cmd = crate::qmd::qmd_command(&cfg.qmd_version);
    cmd.arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Both, and the second one is load-bearing. qmd keeps its
        // collection registry in `$XDG_CONFIG_HOME/qmd/index.yml` and
        // reconciles the index's `store_collections` table against it on
        // startup — so a child pointed at the data root's index but at
        // some *other* config home rewrites that table to match a file
        // that describes a different corpus (or none), and every
        // collection-scoped search then matches nothing. The indexer
        // sets both for the same reason; these two have to agree.
        .env("XDG_CACHE_HOME", qmd_cache_home(&cfg.qmd_root))
        .env("XDG_CONFIG_HOME", qmd_cache_home(&cfg.qmd_root));
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn `{}` (is Node.js installed?)",
            datalib_core::node_runtime::display_command(&cmd)
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("qmd mcp: missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("qmd mcp: missing stdout"))?;
    // Forward qmd's stderr to ours, line-by-line with a `[qmd]` prefix
    // so it's distinguishable from the rest of our process output.
    // Without this, qmd's banner / progress / error lines either fill
    // the pipe buffer and block the child, or get dropped on the floor
    // and make daemon failures opaque. Errors reading from the child's
    // stderr are themselves prefixed and logged — the thread exits
    // silently only when the pipe closes naturally on child exit.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let r = BufReader::new(stderr);
            for line in r.lines() {
                match line {
                    Ok(text) => datalib_obs::status_line!("[qmd] {text}"),
                    Err(e) => {
                        datalib_obs::status_line!("[qmd] (stderr read error: {e})");
                        break;
                    }
                }
            }
        });
    }
    state.child = Some(child);
    state.stdin = Some(stdin);
    state.stdout = Some(BufReader::new(stdout));
    state.next_id = 0;
    state.index_mtime = Some(index_mtime);
    handshake(state).context("qmd mcp handshake failed")?;
    Ok(())
}

fn handshake(state: &mut DaemonState) -> Result<()> {
    state.next_id = state.next_id.wrapping_add(1);
    let id = state.next_id;
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "datalib", "version": "0" },
        },
    });
    send_request(state, &init)?;
    let _ = read_response(state, id)?;
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    });
    send_request(state, &initialized)?;
    Ok(())
}

fn send_request(state: &mut DaemonState, req: &serde_json::Value) -> Result<()> {
    let stdin = state
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("qmd mcp: stdin gone"))?;
    let line = serde_json::to_string(req)?;
    stdin
        .write_all(line.as_bytes())
        .context("write to qmd mcp")?;
    stdin.write_all(b"\n").context("write to qmd mcp")?;
    stdin.flush().context("flush qmd mcp")?;
    Ok(())
}

fn read_response(state: &mut DaemonState, id: u64) -> Result<serde_json::Value> {
    let stdout = state
        .stdout
        .as_mut()
        .ok_or_else(|| anyhow!("qmd mcp: stdout gone"))?;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = stdout.read_line(&mut buf).context("read qmd mcp stdout")?;
        if n == 0 {
            bail!("qmd mcp: stdout closed");
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON banner line; ignore
        };
        // Skip notifications (no `id`) and responses for other ids.
        match v.get("id").and_then(|x| x.as_u64()) {
            Some(got) if got == id => {
                if let Some(err) = v.get("error") {
                    bail!("qmd mcp error: {}", err);
                }
                return Ok(v);
            }
            _ => continue,
        }
    }
}

fn parse_query_response(resp: &serde_json::Value) -> Result<Vec<QmdHit>> {
    let results = resp
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .and_then(|s| s.get("results"))
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            // Include the raw response (truncated) so the next failure
            // tells us exactly what qmd returned instead of just "no
            // structuredContent" — usually the response has an
            // `isError: true` content block with the actual diagnostic.
            let snippet = serde_json::to_string(resp).unwrap_or_else(|_| "<unserializable>".into());
            let snippet = if snippet.len() > 1000 {
                format!(
                    "{}…(truncated, full len {})",
                    &snippet[..1000],
                    snippet.len()
                )
            } else {
                snippet
            };
            anyhow!("qmd mcp: missing result.structuredContent.results; response: {snippet}")
        })?;
    let mut out = Vec::with_capacity(results.len());
    for d in results {
        let raw_file = d.get("file").and_then(|v| v.as_str()).unwrap_or("");
        // `file` is qmd's `displayPath`, built as `collection || '/' ||
        // path` — so the first segment is always the collection name,
        // whatever it is. The CLI returns the same string behind a
        // `qmd://` scheme, so prepending it lets one function strip that
        // segment for both paths. What is left is the file's path
        // relative to the collection's root, which is the data root: the
        // exact string `grid_rows.qmd_path` holds.
        out.push(QmdHit {
            path: strip_uri(&format!("qmd://{raw_file}")).to_string(),
            score: d.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
            snippet: d
                .get("snippet")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            docid: d
                .get("docid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            title: d
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(out)
}

fn teardown(state: &mut DaemonState) {
    state.stdin = None;
    state.stdout = None;
    state.index_mtime = None;
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for QmdDaemon {
    fn drop(&mut self) {
        if let Ok(mut g) = self.state.lock() {
            teardown(&mut g);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_response() {
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "structuredContent": {
                    "results": [
                        {
                            "docid": "#abc",
                            "file": "mirror/slack/x.qmd",
                            "score": 0.42,
                            "snippet": "hi",
                            "title": "X"
                        },
                        {
                            "docid": "#def",
                            "file": "slack_imbue/slack_imbue/render_markdown/y.qmd",
                            "score": 0.1,
                            "snippet": "",
                            "title": ""
                        }
                    ]
                }
            }
        });
        let hits = parse_query_response(&resp).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "slack/x.qmd");
        assert_eq!(hits[0].score, 0.42);
        assert_eq!(hits[0].docid, "#abc");
        // With one collection per group the collection name and the
        // path's first segment are the same word, and only the outer one
        // is stripped: what is left has to be the `qmd_path` a grid row
        // carries, `<group>/render_markdown/…`.
        assert_eq!(hits[1].path, "slack_imbue/render_markdown/y.qmd");
    }

    /// An empty scope is "nothing can match". qmd reads an empty
    /// `collections` array as unscoped and would answer with the whole
    /// corpus, so the request must not be made at all.
    #[test]
    fn empty_scope_matches_nothing() {
        assert!(CollectionScope::Only(Vec::new()).is_empty());
        assert!(!CollectionScope::All.is_empty());
        assert!(!CollectionScope::Only(vec!["a".into()]).is_empty());
        assert_eq!(CollectionScope::All.names(), None);
    }

    #[test]
    fn hybrid_plain_text_sends_lex_and_vec_with_same_query() {
        // No lex syntax → vec gets the same untouched text as lex.
        let s = build_daemon_searches(QueryMode::Hybrid, "earl grey");
        assert_eq!(
            s,
            serde_json::json!([
                {"type": "lex", "query": "earl grey"},
                {"type": "vec", "query": "earl grey"},
            ])
        );
    }

    #[test]
    fn hybrid_quoted_phrase_strips_quotes_from_vec() {
        let s = build_daemon_searches(QueryMode::Hybrid, "\"earl grey\"");
        assert_eq!(
            s,
            serde_json::json!([
                {"type": "lex", "query": "\"earl grey\""},
                {"type": "vec", "query": "earl grey"},
            ])
        );
    }

    #[test]
    fn hybrid_exclusion_only_omits_vec_subquery() {
        // -foo: nothing positive to embed, so we'd be sending an empty
        // vec query. Skip it instead.
        let s = build_daemon_searches(QueryMode::Hybrid, "-spam");
        assert_eq!(
            s,
            serde_json::json!([
                {"type": "lex", "query": "-spam"},
            ])
        );
    }

    #[test]
    fn hybrid_mixed_inclusion_exclusion() {
        let s = build_daemon_searches(QueryMode::Hybrid, "tea -coffee");
        assert_eq!(
            s,
            serde_json::json!([
                {"type": "lex", "query": "tea -coffee"},
                {"type": "vec", "query": "tea"},
            ])
        );
    }

    #[test]
    fn vsearch_strips_lex_syntax_for_vector_query() {
        // Quoted phrase: drop the quotes for vec.
        let s = build_daemon_searches(QueryMode::Vsearch, "\"earl grey\"");
        assert_eq!(
            s,
            serde_json::json!([{"type": "vec", "query": "earl grey"}])
        );
    }

    #[test]
    fn vsearch_exclusion_only_falls_back_to_raw_query() {
        // A pure-exclusion vsearch would strip to empty; rather than
        // send an empty vec query, fall back to the raw text so qmd at
        // least sees *something* to embed. Vector search has no
        // exclusion semantics so this is best-effort.
        let s = build_daemon_searches(QueryMode::Vsearch, "-spam");
        assert_eq!(s, serde_json::json!([{"type": "vec", "query": "-spam"}]));
    }
}
