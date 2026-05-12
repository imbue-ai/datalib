//! Thin wrapper around the `qmd` CLI.
//!
//! Shells out via `npx -y @tobilu/qmd@<version>` — same incantation as
//! `frankweiler_qmd_indexer`. The runner does NOT build the index; it
//! expects one already present at `<root>/.frankweiler/qmd/index.sqlite`.
//!
//! Search modes:
//!   * `query`   — hybrid (BM25 + vectors + reranker). What a user types
//!     into the search bar maps to this.
//!   * `vsearch` — vector-only. Faster, no LLM reranking. Used by the
//!     `qmd_vsearch:"..."` predicate.
//!
//! Output parsing: qmd prints a non-JSON status banner before its JSON
//! array on stdout. We find the first `[` at a line start and parse from
//! there.

use crate::qmd::mapping::{QmdHit, QueryMode};
use crate::qmd::{qmd_cache_home, qmd_index_path};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_QMD_VERSION: &str = "2.1.0";
pub const DEFAULT_COLLECTION: &str = "mirror";

#[derive(Debug, Clone)]
pub struct QmdRunnerConfig {
    /// Data root. Contains the rendered markdown tree AND
    /// `.frankweiler/qmd/index.sqlite`.
    pub qmd_root: PathBuf,
    pub qmd_version: String,
    pub collection: String,
}

impl QmdRunnerConfig {
    pub fn new(qmd_root: impl Into<PathBuf>) -> Self {
        Self {
            qmd_root: qmd_root.into(),
            qmd_version: DEFAULT_QMD_VERSION.into(),
            collection: DEFAULT_COLLECTION.into(),
        }
    }

    pub fn cache_home(&self) -> PathBuf {
        qmd_cache_home(&self.qmd_root)
    }

    pub fn index_path(&self) -> PathBuf {
        qmd_index_path(&self.qmd_root)
    }
}

pub struct QmdRunner {
    cfg: QmdRunnerConfig,
}

impl QmdRunner {
    /// Build a runner. Fails fast if the qmd index isn't materialized at
    /// the canonical path — callers want a clean error, not an opaque
    /// `npx` failure.
    pub fn new(cfg: QmdRunnerConfig) -> Result<Self> {
        let idx = cfg.index_path();
        if !idx.exists() {
            return Err(anyhow!(
                "qmd index not found at {} (run frankweiler-qmd-indexer --root {})",
                idx.display(),
                cfg.qmd_root.display()
            ));
        }
        Ok(Self { cfg })
    }

    pub fn config(&self) -> &QmdRunnerConfig {
        &self.cfg
    }

    pub fn query(&self, q: &str, limit: usize) -> Result<Vec<QmdHit>> {
        self.run("query", q, limit, &["--no-rerank"])
    }

    pub fn vsearch(&self, q: &str, limit: usize) -> Result<Vec<QmdHit>> {
        self.run("vsearch", q, limit, &[])
    }

    pub fn search(&self, mode: QueryMode, q: &str, limit: usize) -> Result<Vec<QmdHit>> {
        match mode {
            QueryMode::Hybrid => self.query(q, limit),
            QueryMode::Vsearch => self.vsearch(q, limit),
        }
    }

    fn run(&self, mode: &str, q: &str, limit: usize, extra: &[&str]) -> Result<Vec<QmdHit>> {
        let pkg = format!("@tobilu/qmd@{}", self.cfg.qmd_version);
        let mut cmd = Command::new("npx");
        cmd.arg("-y")
            .arg(&pkg)
            .arg(mode)
            .arg(q)
            .arg("-n")
            .arg(limit.to_string())
            .arg("--json");
        for a in extra {
            cmd.arg(a);
        }
        cmd.env("XDG_CACHE_HOME", self.cfg.cache_home());
        let out = cmd
            .output()
            .with_context(|| "failed to spawn npx; is Node.js installed?")?;
        if !out.status.success() {
            bail!(
                "qmd {} failed (rc={}): {}",
                mode,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_stdout(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Parse qmd's stdout into `QmdHit`s. qmd prints a status banner before
/// the JSON array; banners can contain `[…]`-shaped fragments, so look
/// for `[` at the start of a line.
pub fn parse_stdout(stdout: &str) -> Result<Vec<QmdHit>> {
    let Some(start) = find_json_start(stdout) else {
        return Ok(Vec::new());
    };
    let data: serde_json::Value =
        serde_json::from_str(&stdout[start..]).context("qmd stdout: invalid JSON")?;
    let arr = data
        .as_array()
        .ok_or_else(|| anyhow!("qmd stdout: expected JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for d in arr {
        let file = d.get("file").and_then(|v| v.as_str()).unwrap_or("");
        out.push(QmdHit {
            path: strip_uri(file).to_string(),
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

fn find_json_start(s: &str) -> Option<usize> {
    // First `[` at the start of a line.
    let bytes = s.as_bytes();
    if bytes.first() == Some(&b'[') {
        return Some(0);
    }
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            return Some(i + 1);
        }
    }
    None
}

/// `qmd://mirror/foo/bar.qmd` → `foo/bar.qmd`.
pub fn strip_uri(uri: &str) -> &str {
    let Some(after_scheme) = uri.strip_prefix("qmd://") else {
        return uri;
    };
    match after_scheme.find('/') {
        Some(i) => &after_scheme[i + 1..],
        None => after_scheme,
    }
}

/// Cache-home for the qmd CLI under a data root. Re-exported for
/// callers that want to set `XDG_CACHE_HOME` themselves (e.g. the
/// indexer binary).
pub fn cache_home_for(root: &Path) -> PathBuf {
    qmd_cache_home(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_uri_handles_collection_prefix() {
        assert_eq!(strip_uri("qmd://mirror/foo/bar.qmd"), "foo/bar.qmd");
        assert_eq!(strip_uri("qmd://other/x"), "x");
        // Not a qmd URI — left alone.
        assert_eq!(strip_uri("plain/path"), "plain/path");
    }

    #[test]
    fn find_json_start_skips_banner() {
        let stdout = "qmd: loading index [0/3]\nready\n[{\"file\":\"qmd://mirror/x\"}]\n";
        let start = find_json_start(stdout).expect("should find array");
        assert_eq!(&stdout[start..start + 1], "[");
    }

    #[test]
    fn parse_stdout_extracts_hits() {
        let stdout = "banner\n[\
            {\"file\":\"qmd://mirror/a.qmd\",\"score\":0.8,\"snippet\":\"hi\",\"docid\":\"d1\",\"title\":\"t\"},\
            {\"file\":\"qmd://mirror/b.qmd\"}\
        ]\n";
        let hits = parse_stdout(stdout).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "a.qmd");
        assert_eq!(hits[0].score, 0.8);
        assert_eq!(hits[0].snippet, "hi");
        assert_eq!(hits[1].path, "b.qmd");
        assert_eq!(hits[1].score, 0.0);
    }

    #[test]
    fn parse_stdout_returns_empty_when_no_json() {
        let hits = parse_stdout("nothing here\n").unwrap();
        assert!(hits.is_empty());
    }
}
