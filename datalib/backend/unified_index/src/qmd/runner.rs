//! Thin wrapper around the `qmd` CLI.

use crate::qmd::mapping::{CollectionScope, QmdHit, QueryMode};
use crate::qmd::{qmd_cache_home, qmd_index_path};
use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

pub use crate::qmd::DEFAULT_QMD_VERSION;

#[derive(Debug, Clone)]
pub struct QmdRunnerConfig {
    /// Data root. Contains the rendered markdown tree AND
    /// `unified_index/qmd/index.sqlite`.
    pub qmd_root: PathBuf,
    pub qmd_version: String,
}

impl QmdRunnerConfig {
    pub fn new(qmd_root: impl Into<PathBuf>) -> Self {
        Self {
            qmd_root: qmd_root.into(),
            qmd_version: DEFAULT_QMD_VERSION.into(),
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
                "qmd index not found at {} — run a sync to build it \
                 (or: DATALIB_DAG_DATA_ROOT={} datalib-step qmd_index)",
                idx.display(),
                cfg.qmd_root.display()
            ));
        }
        Ok(Self { cfg })
    }

    pub fn config(&self) -> &QmdRunnerConfig {
        &self.cfg
    }

    pub fn query(&self, q: &str, limit: usize, scope: &CollectionScope) -> Result<Vec<QmdHit>> {
        let rewritten = build_qmd_query(q);
        self.run("query", &rewritten, limit, scope, &["--no-rerank"])
    }

    pub fn vsearch(&self, q: &str, limit: usize, scope: &CollectionScope) -> Result<Vec<QmdHit>> {
        self.run("vsearch", q, limit, scope, &[])
    }

    pub fn search(
        &self,
        mode: QueryMode,
        q: &str,
        limit: usize,
        scope: &CollectionScope,
    ) -> Result<Vec<QmdHit>> {
        if scope.is_empty() {
            return Ok(Vec::new());
        }
        match mode {
            QueryMode::Hybrid => self.query(q, limit, scope),
            QueryMode::Vsearch => self.vsearch(q, limit, scope),
        }
    }

    fn run(
        &self,
        mode: &str,
        q: &str,
        limit: usize,
        scope: &CollectionScope,
        extra: &[&str],
    ) -> Result<Vec<QmdHit>> {
        let mut cmd = crate::qmd::qmd_command(&self.cfg.qmd_version);
        cmd.arg(mode)
            .arg(q)
            .arg("-n")
            .arg(limit.to_string())
            .arg("--json");
        // `-c` is repeatable and ORs. Omitted entirely for an unscoped
        // search, which qmd answers from its default collection set.
        for name in scope.names().unwrap_or(&[]) {
            cmd.arg("-c").arg(name);
        }
        for a in extra {
            cmd.arg(a);
        }
        // See the note on the daemon's spawn: qmd reconciles the index's
        // `store_collections` against `$XDG_CONFIG_HOME/qmd/index.yml`,
        // so pointing it at the index without also pointing it at that
        // index's own config home rewrites the registry and every
        // collection-scoped search comes back empty.
        cmd.env("XDG_CACHE_HOME", self.cfg.cache_home());
        cmd.env("XDG_CONFIG_HOME", self.cfg.cache_home());
        let out = cmd.output().with_context(|| {
            format!(
                "failed to spawn `{}`; is Node.js installed?",
                datalib_core::node_runtime::display_command(&cmd)
            )
        })?;
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

pub fn strip_uri(uri: &str) -> &str {
    let Some(after_scheme) = uri.strip_prefix("qmd://") else {
        return uri;
    };
    match after_scheme.find('/') {
        Some(i) => &after_scheme[i + 1..],
        None => after_scheme,
    }
}

pub fn build_qmd_query(free_text: &str) -> String {
    if !has_lex_syntax(free_text) {
        return free_text.to_string();
    }
    let mut doc = String::from("lex: ");
    doc.push_str(free_text.trim());
    let plain = strip_lex_syntax(free_text);
    if !plain.is_empty() {
        doc.push_str("\nvec: ");
        doc.push_str(&plain);
    }
    doc
}

pub fn has_lex_syntax(s: &str) -> bool {
    tokenize_query(s)
        .iter()
        .any(|t| t.starts_with('"') || t.starts_with('-'))
}

/// Strip qmd lex syntax from `s`: drop `-`-prefixed tokens entirely
/// (exclusions are meaningless to vector search), strip surrounding
/// quotes from phrases, and rejoin with single spaces. Returns an empty
/// string if every token is an exclusion.
pub fn strip_lex_syntax(s: &str) -> String {
    tokenize_query(s)
        .into_iter()
        .filter(|t| !t.starts_with('-'))
        .map(|t| strip_outer_quotes(&t).to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_outer_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn tokenize_query(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_quote => {
                cur.push('\\');
                escape = true;
            }
            '"' => {
                cur.push('"');
                in_quote = !in_quote;
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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

    #[test]
    fn build_qmd_query_plain_text_passes_through() {
        // No lex-meaningful syntax → qmd's expand-query path stays hot.
        assert_eq!(build_qmd_query("earl grey"), "earl grey");
        assert_eq!(build_qmd_query(""), "");
    }

    #[test]
    fn build_qmd_query_quoted_phrase_emits_query_document() {
        assert_eq!(
            build_qmd_query("\"earl grey\""),
            "lex: \"earl grey\"\nvec: earl grey"
        );
    }

    #[test]
    fn build_qmd_query_phrase_plus_word() {
        assert_eq!(
            build_qmd_query("\"earl grey\" tea"),
            "lex: \"earl grey\" tea\nvec: earl grey tea"
        );
    }

    #[test]
    fn build_qmd_query_negated_word_only_has_no_vec_line() {
        // All tokens are exclusions → vec line would be empty, skip it.
        assert_eq!(build_qmd_query("-spam"), "lex: -spam");
    }

    #[test]
    fn build_qmd_query_negated_phrase_only_has_no_vec_line() {
        assert_eq!(build_qmd_query("-\"earl grey\""), "lex: -\"earl grey\"");
    }

    #[test]
    fn build_qmd_query_mixed_inclusion_and_exclusion() {
        assert_eq!(build_qmd_query("tea -coffee"), "lex: tea -coffee\nvec: tea");
    }

    #[test]
    fn build_qmd_query_phrase_with_exclusion() {
        assert_eq!(
            build_qmd_query("\"earl grey\" -\"chai latte\""),
            "lex: \"earl grey\" -\"chai latte\"\nvec: earl grey"
        );
    }
}
