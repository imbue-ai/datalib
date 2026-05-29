//! End-of-run sync report. Captures per-source extract / translate
//! outcomes, the load summary, and a top-level interrupted flag so a
//! caller (CI, the user, another script) can see what happened on a run
//! that didn't go cleanly. Written to `<data_root>/sync_summary.json`
//! at the end of every run — including on Ctrl-C and on errors that
//! would otherwise abort the binary.
//!
//! We deliberately avoid `serde::Serialize` derives here so the sync
//! crate doesn't need to pull in a `serde` Bazel dep — the structs hold
//! plain fields and we hand-build `serde_json::Value` on write.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
    Skipped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Error => "error",
            Status::Skipped => "skipped",
        }
    }
}

/// Coarse classification used by the orchestrator to decide whether a
/// failure is worth retrying later vs. needing user attention now.
/// Mostly derived from the anyhow error-chain text via [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    RateLimit,
    /// HTTP 5xx from the provider — usually transient.
    ServerError,
    Timeout,
    Network,
    Parse,
    Other,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Auth => "auth",
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::ServerError => "server_error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Network => "network",
            ErrorKind::Parse => "parse",
            ErrorKind::Other => "other",
        }
    }

    /// True for failure modes that are typically transient — a later
    /// run is likely to succeed without user intervention.
    pub fn is_intermittent(self) -> bool {
        matches!(
            self,
            ErrorKind::RateLimit | ErrorKind::ServerError | ErrorKind::Timeout | ErrorKind::Network
        )
    }
}

#[derive(Debug, Clone)]
pub struct PhaseOutcome {
    pub name: String,
    pub type_str: String,
    pub status: Status,
    pub error: Option<String>,
    pub error_kind: Option<ErrorKind>,
    /// Provider-specific one-line stats summary (e.g. `fetched=N
    /// skipped=M ...`). Free-form because providers don't share a
    /// schema.
    pub stats: Option<String>,
}

impl PhaseOutcome {
    pub fn ok(name: &str, type_str: &str, stats: String) -> Self {
        Self {
            name: name.into(),
            type_str: type_str.into(),
            status: Status::Ok,
            error: None,
            error_kind: None,
            stats: Some(stats),
        }
    }

    pub fn err(name: &str, type_str: &str, err: &anyhow::Error) -> Self {
        let chain_text: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(": ");
        let kind = classify(&chain_text);
        Self {
            name: name.into(),
            type_str: type_str.into(),
            status: Status::Error,
            error: Some(chain_text),
            error_kind: Some(kind),
            stats: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "type": self.type_str,
            "status": self.status.as_str(),
            "error": self.error,
            "error_kind": self.error_kind.map(|k| k.as_str()),
            "intermittent": self.error_kind.map(|k| k.is_intermittent()),
            "stats": self.stats,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub documents_loaded: usize,
    pub documents_total: usize,
    pub rows_inserted: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_secs: Option<f64>,
    pub interrupted: bool,
    pub fatal_error: Option<String>,
    pub data_root: Option<PathBuf>,
    /// Where to write the summary JSON. Filled in once the orchestrator
    /// has resolved the data_root so the Ctrl-C handler can flush even
    /// before the run finishes.
    pub output_path: Option<PathBuf>,
    pub extract: Vec<PhaseOutcome>,
    pub translate: Vec<PhaseOutcome>,
    pub load: Option<LoadOutcome>,
    pub qmd_index: Option<PhaseOutcome>,
    /// Raw stdout of `qmd status` captured at the end of the qmd-index
    /// phase. `None` if the phase was skipped or the capture failed.
    /// qmd has no `--json` mode for `status`, so this is human-readable
    /// text.
    pub qmd_status: Option<String>,
}

impl SyncSummary {
    pub fn new() -> Self {
        Self {
            started_at: Utc_now_rfc3339(),
            finished_at: None,
            duration_secs: None,
            interrupted: false,
            fatal_error: None,
            data_root: None,
            output_path: None,
            extract: Vec::new(),
            translate: Vec::new(),
            load: None,
            qmd_index: None,
            qmd_status: None,
        }
    }

    pub fn finalize(&mut self, start: std::time::Instant) {
        self.finished_at = Some(Utc_now_rfc3339());
        self.duration_secs = Some(start.elapsed().as_secs_f64());
    }

    pub fn to_json(&self) -> Value {
        let load = self.load.as_ref().map(|l| {
            json!({
                "documents_loaded": l.documents_loaded,
                "documents_total": l.documents_total,
                "rows_inserted": l.rows_inserted,
                "error": l.error,
            })
        });
        let any_error = self.fatal_error.is_some()
            || self.extract.iter().any(|o| o.status == Status::Error)
            || self.translate.iter().any(|o| o.status == Status::Error)
            || self.load.as_ref().is_some_and(|l| l.error.is_some())
            || self
                .qmd_index
                .as_ref()
                .is_some_and(|o| o.status == Status::Error);
        let overall = if self.interrupted {
            "interrupted"
        } else if self.fatal_error.is_some() {
            "fatal_error"
        } else if any_error {
            "partial"
        } else {
            "ok"
        };
        json!({
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "duration_secs": self.duration_secs,
            "interrupted": self.interrupted,
            "fatal_error": self.fatal_error,
            "overall_status": overall,
            "data_root": self.data_root.as_ref().map(|p| p.display().to_string()),
            "extract": self.extract.iter().map(PhaseOutcome::to_json).collect::<Vec<_>>(),
            "translate": self.translate.iter().map(PhaseOutcome::to_json).collect::<Vec<_>>(),
            "load": load,
            "qmd_index": self.qmd_index.as_ref().map(PhaseOutcome::to_json),
            "qmd_status": self.qmd_status,
        })
    }

    /// Write the summary to its configured `output_path`. No-op if the
    /// path hasn't been set yet (e.g. Ctrl-C before config load).
    pub fn write(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(path) = self.output_path.clone() else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = self.to_json();
        let text = serde_json::to_string_pretty(&json)?;
        std::fs::write(&path, text)?;
        Ok(Some(path))
    }
}

#[allow(non_snake_case)]
fn Utc_now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Heuristic mapping of an anyhow chain text to an [`ErrorKind`].
/// Matches in priority order so that, e.g., an HTTP 401 wins over a
/// network error mention later in the chain.
pub fn classify(chain_text: &str) -> ErrorKind {
    let s = chain_text;
    if s.contains("HTTP 401")
        || s.contains("HTTP 403")
        || s.contains("Unauthorized")
        || s.contains("Forbidden")
        || s.contains("cf-mitigated=Some(")
    {
        return ErrorKind::Auth;
    }
    if s.contains("rate-limited") || s.contains("HTTP 429") {
        return ErrorKind::RateLimit;
    }
    if s.contains("HTTP 500")
        || s.contains("HTTP 502")
        || s.contains("HTTP 503")
        || s.contains("HTTP 504")
        || s.contains("Request timeout")
    {
        return ErrorKind::ServerError;
    }
    if s.contains("timed out") || s.contains("timeout") {
        return ErrorKind::Timeout;
    }
    if s.contains("connection") || s.contains("dns") || s.contains("resolve") {
        return ErrorKind::Network;
    }
    if s.contains("parse") || s.contains("deserialize") || s.contains("JSON") {
        return ErrorKind::Parse;
    }
    ErrorKind::Other
}

/// Convenience: build a `PhaseOutcome` from a `Result<String>` produced
/// by a per-source extract/translate driver.
pub fn outcome_from(name: &str, type_str: &str, r: Result<String, anyhow::Error>) -> PhaseOutcome {
    match r {
        Ok(stats) => PhaseOutcome::ok(name, type_str, stats),
        Err(e) => PhaseOutcome::err(name, type_str, &e),
    }
}

/// Render a path the user can open from the terminal.
pub fn pretty_path(p: &Path) -> String {
    p.display().to_string()
}
