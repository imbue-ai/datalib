//! Long-lived `qmd mcp` subprocess.
//!
//! Per-search shell-outs to `npx -y @tobilu/qmd@<ver> query …` were
//! costing ~4s/call: Node startup, model reload, index open. The MCP
//! server keeps all of that resident; first call still pays the model
//! load, every subsequent call is sub-second.
//!
//! Protocol: JSON-RPC over the child's stdio, one message per line.
//! The MCP handshake (`initialize` + `notifications/initialized`) runs
//! once on the first request. We hold the child for the lifetime of
//! the daemon and respawn lazily on any I/O error.
//!
//! Concurrency: a single child is shared behind a `std::sync::Mutex`.
//! MCP-over-stdio is request/response per session, so serializing here
//! is necessary; callers run inside `tokio::task::spawn_blocking` so
//! the tokio runtime is never blocked.

use crate::qmd::mapping::{QmdHit, QueryMode};
use crate::qmd::runner::{strip_uri, DEFAULT_COLLECTION, DEFAULT_QMD_VERSION};
use crate::qmd::{qmd_cache_home, qmd_index_path};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::thread;

#[derive(Debug, Clone)]
pub struct QmdDaemonConfig {
    pub qmd_root: PathBuf,
    pub qmd_version: String,
    pub collection: String,
}

impl QmdDaemonConfig {
    pub fn new(qmd_root: impl Into<PathBuf>) -> Self {
        Self {
            qmd_root: qmd_root.into(),
            qmd_version: DEFAULT_QMD_VERSION.into(),
            collection: DEFAULT_COLLECTION.into(),
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
}

impl QmdDaemon {
    /// Build a daemon handle. The child isn't spawned yet — that happens
    /// on the first query so we don't pay startup cost when nobody is
    /// searching.
    pub fn new(cfg: QmdDaemonConfig) -> Result<Self> {
        let idx = qmd_index_path(&cfg.qmd_root);
        if !idx.exists() {
            return Err(anyhow!(
                "qmd index not found at {} — run the indexer first",
                idx.display()
            ));
        }
        Ok(Self {
            cfg,
            state: Mutex::new(DaemonState {
                child: None,
                stdin: None,
                stdout: None,
                next_id: 0,
            }),
        })
    }

    pub fn config(&self) -> &QmdDaemonConfig {
        &self.cfg
    }

    /// Run a search. On any I/O error the child is torn down so the next
    /// call respawns cleanly; the caller decides whether to fall back to
    /// the CLI path.
    pub fn search(&self, mode: QueryMode, q: &str, limit: usize) -> Result<Vec<QmdHit>> {
        // The MCP `query` tool requires typed sub-queries — there's no
        // bare auto-expand entry point like the CLI's `qmd query "<text>"`.
        // For Hybrid we send lex+vec (qmd's own "best recall" recipe);
        // for Vsearch we send vec only. First sub-query gets 2× weight,
        // so lex goes first when present (better behavior on exact terms
        // like UUIDs, channel names, usernames).
        let searches = match mode {
            QueryMode::Hybrid => serde_json::json!([
                {"type": "lex", "query": q},
                {"type": "vec", "query": q},
            ]),
            QueryMode::Vsearch => serde_json::json!([
                {"type": "vec", "query": q},
            ]),
        };
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("daemon mutex poisoned"))?;
        let res = (|| -> Result<Vec<QmdHit>> {
            ensure_started(&mut guard, &self.cfg)?;
            guard.next_id = guard.next_id.wrapping_add(1);
            let id = guard.next_id;
            let req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "query",
                    "arguments": {
                        "searches": searches,
                        "limit": limit,
                        "rerank": false,
                    },
                },
            });
            send_request(&mut guard, &req)?;
            let resp = read_response(&mut guard, id)?;
            parse_query_response(&resp, &self.cfg.collection)
        })();
        if res.is_err() {
            teardown(&mut guard);
        }
        res
    }
}

fn ensure_started(state: &mut DaemonState, cfg: &QmdDaemonConfig) -> Result<()> {
    // If we have a child, make sure it's still alive — `try_wait`
    // returns `Some(_)` if the process exited.
    if let Some(child) = state.child.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => teardown(state),
            Ok(None) => return Ok(()),
            Err(_) => teardown(state),
        }
    }
    spawn(state, cfg)
}

fn spawn(state: &mut DaemonState, cfg: &QmdDaemonConfig) -> Result<()> {
    let pkg = format!("@tobilu/qmd@{}", cfg.qmd_version);
    let mut cmd = Command::new("npx");
    cmd.arg("-y")
        .arg(&pkg)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("XDG_CACHE_HOME", qmd_cache_home(&cfg.qmd_root));
    let mut child = cmd
        .spawn()
        .with_context(|| "failed to spawn `npx … qmd mcp` (is Node.js installed?)")?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("qmd mcp: missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("qmd mcp: missing stdout"))?;
    // Drain stderr in a background thread. Without this, qmd's banner
    // and progress output eventually fills the pipe buffer and blocks
    // the child. Lines are dropped on the floor — they're noise we
    // don't surface anywhere useful.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let r = BufReader::new(stderr);
            for _ in r.lines().map_while(Result::ok) {}
        });
    }
    state.child = Some(child);
    state.stdin = Some(stdin);
    state.stdout = Some(BufReader::new(stdout));
    state.next_id = 0;
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
            "clientInfo": { "name": "frankweiler", "version": "0" },
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

/// Read until we see a JSON-RPC response with our id. MCP servers may
/// emit unrelated notifications (logs, progress) on the same channel,
/// so we skip non-matching messages.
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

fn parse_query_response(resp: &serde_json::Value, collection: &str) -> Result<Vec<QmdHit>> {
    let results = resp
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .and_then(|s| s.get("results"))
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow!("qmd mcp: missing result.structuredContent.results"))?;
    let mut out = Vec::with_capacity(results.len());
    for d in results {
        let raw_file = d.get("file").and_then(|v| v.as_str()).unwrap_or("");
        // MCP paths look like `mirror/slack/...` — same shape as the
        // CLI's URI minus the `qmd://` scheme. Re-prepending the scheme
        // lets us reuse the same prefix-strip logic.
        let with_scheme = if raw_file.starts_with(&format!("{collection}/")) {
            format!("qmd://{raw_file}")
        } else {
            raw_file.to_string()
        };
        out.push(QmdHit {
            path: strip_uri(&with_scheme).to_string(),
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
                            "file": "other/foo.qmd",
                            "score": 0.1,
                            "snippet": "",
                            "title": ""
                        }
                    ]
                }
            }
        });
        let hits = parse_query_response(&resp, "mirror").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "slack/x.qmd");
        assert_eq!(hits[0].score, 0.42);
        assert_eq!(hits[0].docid, "#abc");
        // Path that doesn't start with the configured collection is left
        // untouched (defensive — shouldn't happen in practice).
        assert_eq!(hits[1].path, "other/foo.qmd");
    }
}
