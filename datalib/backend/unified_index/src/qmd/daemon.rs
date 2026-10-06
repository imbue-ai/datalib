//! Long-lived `qmd mcp` subprocess.

use crate::qmd::lex::{has_lex_syntax, strip_lex_syntax};
use crate::qmd::mapping::{CollectionScope, QmdHit, QueryMode};
use crate::qmd::DEFAULT_QMD_VERSION;
use crate::qmd::{qmd_cache_home, qmd_index_path};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long one search may wait on qmd, spawn and handshake included.
/// Under the gateway's 30s (`APPLET_READ_TIMEOUT` in datalib-http), so a
/// qmd that never answers costs this search an error naming it, and the
/// next search a fresh child — not every search after it a bare proxy
/// timeout, which is what holding the daemon's lock on a hung read did.
pub const QMD_ANSWER_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct QmdDaemonConfig {
    pub qmd_root: PathBuf,
    pub qmd_version: String,
    pub answer_deadline: Duration,
    /// A shell script run in place of `qmd mcp`.
    #[cfg(test)]
    fake_qmd: Option<String>,
}

impl QmdDaemonConfig {
    pub fn new(qmd_root: impl Into<PathBuf>) -> Self {
        Self {
            qmd_root: qmd_root.into(),
            qmd_version: DEFAULT_QMD_VERSION.into(),
            answer_deadline: QMD_ANSWER_DEADLINE,
            #[cfg(test)]
            fake_qmd: None,
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
    /// qmd's stdout a line at a time, read on a thread of its own so that
    /// waiting for an answer can give up.
    stdout: Option<Receiver<String>>,
    next_id: u64,
    /// mtime of the index the live child was spawned against. When the
    /// index on disk is newer (a sync rebuilt it), the child holds a
    /// stale view and must be respawned. `None` when no child is live.
    index_mtime: Option<SystemTime>,
    started_at: Option<Instant>,
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
                started_at: None,
            }),
        }
    }

    pub fn config(&self) -> &QmdDaemonConfig {
        &self.cfg
    }

    /// Run a search. On any error, a missed deadline included, the child
    /// is torn down so the next call respawns cleanly.
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
        // Taken after the lock: a search queued behind another gets the
        // whole allowance for its own exchange.
        let deadline = Instant::now() + self.cfg.answer_deadline;
        let res = (|| -> Result<Vec<QmdHit>> {
            // The index may be absent (no sync yet) or freshly rebuilt.
            // Its mtime both gates the search and tells `ensure_started`
            // whether the live child is stale. A missing index is an
            // answer ("sync to build it"), not a daemon failure.
            let idx = qmd_index_path(&self.cfg.qmd_root);
            let index_mtime = std::fs::metadata(&idx)
                .and_then(|m| m.modified())
                .map_err(|_| {
                    anyhow!(
                        "qmd index not found at {} — sync to build it",
                        idx.display()
                    )
                })?;
            ensure_started(&mut guard, &self.cfg, index_mtime, deadline)?;
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
            let sent = Instant::now();
            send_request(&mut guard, &req)?;
            let resp = read_response(&guard, id, deadline)?;
            let hits = parse_query_response(&resp)?;
            tracing::info!(
                request = id,
                ms = sent.elapsed().as_millis() as u64,
                hits = hits.len(),
                "qmd answered a search"
            );
            Ok(hits)
        })();
        if let Err(e) = &res {
            let pid = guard.child.as_ref().map(Child::id);
            if let (Some(pid), Some(_)) = (pid, e.downcast_ref::<NoAnswer>()) {
                ask_for_report(pid, &reports_dir(&self.cfg.qmd_root));
            }
            tracing::error!(pid, error = %format!("{e:#}"), "a qmd search failed; stopping qmd");
            teardown(&mut guard);
        }
        res
    }
}

/// Build the MCP `searches` JSON array for a given user query and mode.
/// Extracted so the lex/vec routing is unit-testable without spawning
/// the daemon. The lex rules are [`crate::qmd::lex`]'s.
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
    deadline: Instant,
) -> Result<()> {
    // A rebuilt index (mtime moved) means the resident child opened a
    // now-stale copy — tear it down so we respawn against the fresh
    // file. `teardown` clears `index_mtime`, so the checks below fall
    // through to a clean spawn.
    if state.index_mtime.is_some_and(|m| m != index_mtime) {
        tracing::info!("the qmd index was rebuilt; restarting qmd");
        teardown(state);
    }
    // If we still have a child, make sure it's alive — `try_wait`
    // returns `Some(_)` if the process exited.
    if let Some(child) = state.child.as_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::error!(
                    pid = child.id(),
                    %status,
                    "qmd exited on its own; starting another"
                );
                teardown(state);
            }
            Ok(None) => return Ok(()),
            Err(_) => teardown(state),
        }
    }
    spawn(state, cfg, index_mtime, deadline)
}

fn qmd_mcp_command(cfg: &QmdDaemonConfig) -> Result<std::process::Command> {
    #[cfg(test)]
    if let Some(script) = &cfg.fake_qmd {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        return Ok(cmd);
    }
    let mut cmd = crate::qmd::qmd_command(&cfg.qmd_version)?;
    cmd.arg("mcp");
    let reports = reports_dir(&cfg.qmd_root);
    match std::fs::create_dir_all(&reports) {
        Ok(()) => match node_options(&reports, std::env::var("NODE_OPTIONS").ok().as_deref()) {
            Some(options) => {
                cmd.env("NODE_OPTIONS", options);
            }
            None => tracing::warn!(
                dir = %reports.display(),
                "qmd cannot be asked for a diagnostic report: the path does not fit NODE_OPTIONS"
            ),
        },
        Err(e) => tracing::warn!(
            dir = %reports.display(),
            error = %e,
            "qmd cannot be asked for a diagnostic report: its directory could not be made"
        ),
    }
    Ok(cmd)
}

fn spawn(
    state: &mut DaemonState,
    cfg: &QmdDaemonConfig,
    index_mtime: SystemTime,
    deadline: Instant,
) -> Result<()> {
    let mut cmd = qmd_mcp_command(cfg)?;
    cmd.stdin(Stdio::piped())
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
    let pid = child.id();
    // Drained, or qmd blocks on a full pipe; logged at `warn`, because a
    // qmd that is working says nothing there.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(text) => tracing::warn!(pid, stream = "stderr", "qmd: {}", clip(&text)),
                    Err(e) => {
                        tracing::warn!(pid, error = %e, "could not read qmd's stderr");
                        break;
                    }
                }
            }
        });
    }
    let (lines_tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(text) => {
                    if lines_tx.send(text).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(pid, error = %e, "could not read qmd's stdout");
                    break;
                }
            }
        }
    });
    let started = Instant::now();
    state.child = Some(child);
    state.stdin = Some(stdin);
    state.stdout = Some(lines);
    state.next_id = 0;
    state.index_mtime = Some(index_mtime);
    state.started_at = Some(started);
    handshake(state, deadline).context("qmd mcp handshake failed")?;
    tracing::info!(
        pid,
        handshake_ms = started.elapsed().as_millis() as u64,
        "started qmd"
    );
    Ok(())
}

fn handshake(state: &mut DaemonState, deadline: Instant) -> Result<()> {
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
    let _ = read_response(state, id, deadline)?;
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

fn read_response(state: &DaemonState, id: u64, deadline: Instant) -> Result<serde_json::Value> {
    let pid = state.child.as_ref().map(Child::id);
    let started_at = state.started_at;
    let lines = state
        .stdout
        .as_ref()
        .ok_or_else(|| anyhow!("qmd mcp: stdout gone"))?;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match lines.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                return Err(NoAnswer {
                    request: id,
                    pid,
                    up_secs: started_at.map_or(0, |t| t.elapsed().as_secs()),
                }
                .into())
            }
            Err(RecvTimeoutError::Disconnected) => bail!("qmd mcp: stdout closed"),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            tracing::warn!(pid, stream = "stdout", "qmd: {}", clip(trimmed));
            continue;
        };
        match v.get("id").and_then(|x| x.as_u64()) {
            Some(got) if got == id => {
                if let Some(err) = v.get("error") {
                    bail!("qmd mcp error: {}", err);
                }
                return Ok(v);
            }
            Some(other) => tracing::warn!(
                pid,
                request = other,
                waiting_for = id,
                "qmd answered a request nobody is waiting for"
            ),
            None => tracing::info!(pid, notification = clip(trimmed), "qmd sent a notification"),
        }
    }
}

/// qmd did not answer before the search's deadline.
#[derive(Debug)]
struct NoAnswer {
    request: u64,
    pid: Option<u32>,
    up_secs: u64,
}

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pid = self.pid.map_or("?".into(), |p| p.to_string());
        write!(
            f,
            "qmd (pid {pid}, up {}s) did not answer request {} in time; it was stopped, \
             and the next search starts a fresh one",
            self.up_secs, self.request
        )
    }
}

impl std::error::Error for NoAnswer {}

/// Where qmd writes a Node diagnostic report when asked, which is what a
/// hang leaves behind to say what it was waiting on.
fn reports_dir(qmd_root: &Path) -> PathBuf {
    qmd_cache_home(qmd_root).join("reports")
}

const REPORTS_KEPT: usize = 20;
const REPORT_WAIT: Duration = Duration::from_secs(2);

/// `NODE_OPTIONS` for a qmd that writes a report into `dir` on SIGUSR2,
/// after whatever the environment already asked for. The report leaves
/// out the environment, which carries secrets and would otherwise land in
/// the data root. `None` for a path NODE_OPTIONS' quoting cannot carry.
fn node_options(dir: &Path, inherited: Option<&str>) -> Option<String> {
    let dir = dir.to_str().filter(|d| !d.contains('"'))?;
    let ours = format!(
        "--report-exclude-env --report-on-signal --report-signal=SIGUSR2 --report-compact --report-directory=\"{dir}\""
    );
    Some(match inherited.map(str::trim) {
        Some(before) if !before.is_empty() => format!("{before} {ours}"),
        _ => ours,
    })
}

/// Asks a hung qmd for its diagnostic report and logs what it says,
/// before the caller kills it. Bounded: a process too far gone to write
/// one costs [`REPORT_WAIT`] and a line saying so.
fn ask_for_report(pid: u32, dir: &Path) {
    let before = report_files(dir);
    // SAFETY: kill(2) takes no pointers; `pid` is our child, not yet reaped.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR2) } != 0 {
        tracing::warn!(pid, error = %std::io::Error::last_os_error(), "could not signal qmd for a report");
        return;
    }
    let marker = format!(".{pid}.");
    let deadline = Instant::now() + REPORT_WAIT;
    let found = loop {
        let written = report_files(dir).into_iter().find(|p| {
            !before.contains(p)
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(&marker))
        });
        // A report is one write; one that does not parse yet is still
        // arriving.
        let parsed = written.and_then(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some((p, serde_json::from_str::<serde_json::Value>(&text).ok()?))
        });
        if parsed.is_some() || Instant::now() >= deadline {
            break parsed;
        }
        thread::sleep(Duration::from_millis(50));
    };
    match found {
        Some((path, report)) => {
            let summary = summarize_report(&report);
            tracing::error!(
                pid,
                report = %path.display(),
                js_stack = %summary.js_stack,
                active_handles = %summary.active_handles,
                "qmd hung; its diagnostic report says what it was doing"
            );
        }
        None => {
            tracing::warn!(pid, dir = %dir.display(), "qmd hung and wrote no diagnostic report")
        }
    }
    prune_reports(dir);
}

fn report_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("report."))
        })
        .collect();
    // Node names a report `report.<date>.<time>.<pid>…`, so name order is
    // time order.
    files.sort();
    files
}

fn prune_reports(dir: &Path) {
    let files = report_files(dir);
    let excess = files.len().saturating_sub(REPORTS_KEPT);
    for old in &files[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

#[derive(Debug, PartialEq)]
struct ReportSummary {
    js_stack: String,
    active_handles: String,
}

/// What a Node diagnostic report says a stuck process was doing: its
/// JavaScript stack, and the libuv handles still active, counted by type.
/// A process idle with nothing but its pipes open is waiting on a promise
/// nothing will settle; one with a timer or a request is waiting on that.
fn summarize_report(report: &serde_json::Value) -> ReportSummary {
    const IDLE: &str = "none: no JavaScript was running";
    let js_stack = report
        .pointer("/javascriptStack/stack")
        .and_then(|s| s.as_array())
        .map(|frames| {
            frames
                .iter()
                .filter_map(|f| f.as_str())
                .take(12)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .filter(|stack| !stack.is_empty())
        .unwrap_or_else(|| IDLE.to_string());
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for handle in report
        .get("libuv")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
    {
        if handle.get("is_active").and_then(|a| a.as_bool()) == Some(true) {
            let kind = handle.get("type").and_then(|t| t.as_str()).unwrap_or("?");
            *counts.entry(kind.to_string()).or_default() += 1;
        }
    }
    let active_handles = counts
        .iter()
        .map(|(kind, n)| format!("{kind}×{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    ReportSummary {
        js_stack,
        active_handles,
    }
}

/// A line of qmd's output cut to a length the log can hold.
fn clip(line: &str) -> &str {
    const MAX: usize = 2_000;
    match line.char_indices().nth(MAX) {
        Some((at, _)) => &line[..at],
        None => line,
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

/// A hit's `qmd://<collection>/<path>` as the `<path>` a grid row's
/// `qmd_path` holds.
pub fn strip_uri(uri: &str) -> &str {
    let Some(after_scheme) = uri.strip_prefix("qmd://") else {
        return uri;
    };
    match after_scheme.find('/') {
        Some(i) => &after_scheme[i + 1..],
        None => after_scheme,
    }
}

fn teardown(state: &mut DaemonState) {
    state.stdin = None;
    state.stdout = None;
    state.index_mtime = None;
    let started_at = state.started_at.take();
    if let Some(mut child) = state.child.take() {
        let pid = child.id();
        let _ = child.kill();
        let status = child
            .wait()
            .map_or_else(|e| format!("wait failed: {e}"), |s| s.to_string());
        tracing::info!(
            pid,
            up_secs = started_at.map_or(0, |t| t.elapsed().as_secs()),
            %status,
            "stopped qmd"
        );
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
    fn strip_uri_handles_collection_prefix() {
        assert_eq!(strip_uri("qmd://mirror/foo/bar.qmd"), "foo/bar.qmd");
        assert_eq!(strip_uri("qmd://other/x"), "x");
        // A collection alone has no path under it.
        assert_eq!(strip_uri("qmd://mirror"), "mirror");
        // Not a qmd URI — left alone.
        assert_eq!(strip_uri("plain/path"), "plain/path");
    }

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

    /// A daemon whose `qmd mcp` is the shell script `script`, over a root
    /// with an index file for it to find.
    fn fake_daemon(root: &std::path::Path, script: &str, deadline: Duration) -> QmdDaemon {
        let idx = qmd_index_path(root);
        std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
        std::fs::write(&idx, b"").unwrap();
        let mut cfg = QmdDaemonConfig::new(root);
        cfg.answer_deadline = deadline;
        cfg.fake_qmd = Some(script.to_string());
        QmdDaemon::new(cfg)
    }

    /// Answers `initialize`, then reads `initialized` and the query.
    const HANDSHAKE: &str =
        r#"read l; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; read l; read l;"#;
    /// Answers the query, with a banner and a notification ahead of it.
    const ANSWER: &str = r#"echo 'a banner line'; echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'; echo '{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"results":[{"file":"slack/slack/a.md","score":0.5}]}}}'; exec sleep 60"#;

    /// The regression: a qmd that took a query and never answered held the
    /// daemon's lock on a blocking read, so every search after it hung
    /// until the applet restarted. Now it costs that one search an error,
    /// and the next search gets a fresh qmd.
    #[test]
    fn a_qmd_that_never_answers_costs_one_search() {
        let td = tempfile::tempdir().unwrap();
        let marker = td.path().join("hung-once");
        let script = format!(
            "{HANDSHAKE} if [ -e '{m}' ]; then {ANSWER}; else touch '{m}'; exec sleep 60; fi",
            m = marker.display()
        );
        let daemon = fake_daemon(td.path(), &script, Duration::from_millis(500));

        let t0 = Instant::now();
        let err = daemon
            .search(QueryMode::Hybrid, "balcony", 10, &CollectionScope::All)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("did not answer request 2 in time"),
            "{err:#}"
        );
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());

        let hits = daemon
            .search(QueryMode::Hybrid, "balcony", 10, &CollectionScope::All)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["slack/a.md"]);
    }

    /// The report a hung qmd leaves is read down to what says why: the
    /// JavaScript stack, and the handles still active, counted by type —
    /// inactive ones are not waiting on anything.
    #[test]
    fn a_report_is_summarized_to_its_stack_and_active_handles() {
        let report = serde_json::json!({
            "javascriptStack": {"stack": ["at embed (llm.js:1)", "at query (server.js:2)"]},
            "libuv": [
                {"type": "pipe", "is_active": true},
                {"type": "pipe", "is_active": true},
                {"type": "timer", "is_active": true},
                {"type": "tcp", "is_active": false},
            ],
        });
        assert_eq!(
            summarize_report(&report),
            ReportSummary {
                js_stack: "at embed (llm.js:1) | at query (server.js:2)".into(),
                active_handles: "pipe×2, timer×1".into(),
            }
        );
    }

    /// A report taken while the event loop sat idle has no JavaScript stack
    /// at all, which is itself the answer: qmd was waiting, not working.
    #[test]
    fn an_idle_report_says_no_javascript_was_running() {
        let report = serde_json::json!({
            "javascriptStack": {"message": "", "errorProperties": {}},
            "libuv": [{"type": "pipe", "is_active": true}],
        });
        let summary = summarize_report(&report);
        assert_eq!(summary.js_stack, "none: no JavaScript was running");
        assert_eq!(summary.active_handles, "pipe×1");
    }

    /// The report switch goes after whatever NODE_OPTIONS already held,
    /// with the directory quoted so a data root with spaces survives.
    #[test]
    fn node_options_add_the_report_switch_to_what_was_there() {
        let dir = Path::new("/a root/qmd/reports");
        let ours = r#"--report-exclude-env --report-on-signal --report-signal=SIGUSR2 --report-compact --report-directory="/a root/qmd/reports""#;
        assert_eq!(node_options(dir, None).as_deref(), Some(ours));
        assert_eq!(
            node_options(dir, Some("--max-old-space-size=4096")),
            Some(format!("--max-old-space-size=4096 {ours}"))
        );
        assert_eq!(node_options(Path::new("/a\"b"), None), None);
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
    fn an_escaped_quote_stays_inside_its_phrase() {
        let v = build_daemon_searches(QueryMode::Hybrid, r#""a \"quoted\" word" tea"#);
        assert_eq!(v[1]["query"], r#"a \"quoted\" word tea"#);
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
