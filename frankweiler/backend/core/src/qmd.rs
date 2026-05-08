//! QMD scanner / parser.
//!
//! The ingest pipeline writes one .qmd per conversation under
//! `<root>/<provider>/<account>/llm_chats/<conversation_id>__<slug>.qmd`,
//! where `<provider>` is `anthropic` or `openai`. The frontmatter shape
//! differs slightly between providers (anthropic uses `uuid` /
//! `account_uuid` / `created_at`, openai uses `id` / `account_id` /
//! `create_time`); we accept both via serde aliases so a single
//! `Frontmatter` covers both.
//!
//! Dolt is the source of truth, but the QMDs are the search index.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    #[serde(default)]
    pub provider: String,
    #[serde(default, alias = "id")]
    pub uuid: String,
    #[serde(default, alias = "title")]
    pub name: Option<String>,
    #[serde(default, alias = "account_id")]
    pub account_uuid: Option<String>,
    #[serde(default)]
    pub project_uuid: Option<String>,
    #[serde(default, alias = "create_time")]
    pub created_at: Option<String>,
    #[serde(default, alias = "update_time")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub sender: String,
    pub when: Option<String>,
    pub model: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub messages: Vec<Message>,
}

#[derive(Debug, thiserror::Error)]
pub enum QmdError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("missing frontmatter in {0}")]
    MissingFrontmatter(PathBuf),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// Parse a single QMD file.
pub fn parse_qmd(path: &Path) -> Result<Conversation, QmdError> {
    let text = fs::read_to_string(path)?;
    let (fm, body) =
        split_frontmatter(&text).ok_or_else(|| QmdError::MissingFrontmatter(path.to_path_buf()))?;
    let frontmatter: Frontmatter = serde_yaml::from_str(fm)?;
    let messages = parse_messages(body);
    Ok(Conversation {
        path: path.to_path_buf(),
        frontmatter,
        messages,
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let fm = &rest[..end];
    let after = &rest[end + 4..];
    // Skip the newline that follows the closing `---` (if present).
    let body = after.strip_prefix('\n').unwrap_or(after);
    Some((fm, body))
}

/// Body is `# Title\n\n## Sender\n\n*ts[ · model]*\n\n<text>\n\n## Sender\n...`.
/// The OpenAI renderer joins extra metadata (model slug) onto the timestamp
/// line with " · "; we split the first segment off as `when` and stash the
/// rest as `model` so the UI can show e.g. "gpt-4o" in the Author column.
fn parse_messages(body: &str) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut iter = body.split("\n## ").peekable();
    // First chunk before the first `## ` is the H1 title block — skip it.
    iter.next();
    for chunk in iter {
        // Chunk: "Sender\n\n*timestamp*\n\nbody…\n\n"
        let mut lines = chunk.lines();
        let sender = lines.next().unwrap_or("").trim().to_string();
        let mut when: Option<String> = None;
        let mut model: Option<String> = None;
        let mut content_lines: Vec<&str> = Vec::new();
        let mut consumed_ts = false;
        for line in lines {
            if !consumed_ts {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if let Some(ts) = t.strip_prefix('*').and_then(|s| s.strip_suffix('*')) {
                    let mut bits = ts.splitn(2, " · ");
                    when = bits.next().map(str::trim).map(str::to_owned);
                    model = bits.next().map(str::trim).map(str::to_owned);
                    consumed_ts = true;
                    continue;
                }
                consumed_ts = true;
            }
            content_lines.push(line);
        }
        let text = content_lines.join("\n").trim().to_string();
        // Inherit timestamp from the previous message if this one has none —
        // tool/system messages routinely arrive timestampless. The Python
        // renderer does this with a microsecond bump for sort stability;
        // this is the file-level safety net for QMDs that pre-date that.
        if when.is_none() {
            when = out.last().and_then(|m| m.when.clone());
        }
        out.push(Message {
            sender,
            when,
            model,
            text,
        });
    }
    out
}

/// Scan `<root>/{anthropic,openai}/*/llm_chats/*.qmd`. Missing dirs yield
/// an empty Vec.
pub fn scan_root(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for provider in ["anthropic", "openai"] {
        let provider_dir = root.join(provider);
        let Ok(accounts) = fs::read_dir(&provider_dir) else {
            continue;
        };
        for acct in accounts.flatten() {
            let chats = acct.path().join("llm_chats");
            let Ok(files) = fs::read_dir(&chats) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == "qmd") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

pub fn load_all(root: &Path) -> Vec<Conversation> {
    scan_root(root)
        .into_iter()
        .filter_map(|p| match parse_qmd(&p) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("frankweiler: skipping {}: {}", p.display(), e);
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    const SAMPLE: &str = "---
provider: anthropic
uuid: abc-123
name: \"Treemap layout\"
account_uuid: acct-1
project_uuid: proj-1
created_at: '2025-04-10 14:00:00'
updated_at: '2025-04-10 15:00:00'
summary: about treemaps
---

# Treemap layout

## Human

*2025-04-10 14:00:00*

How do I lay out a treemap?

## Assistant

*2025-04-10 14:01:00*

You can use squarified treemap.

Multiple paragraphs work.
";

    #[test]
    fn parses_frontmatter_and_messages() {
        let dir = tempdir();
        let p = dir.join("test.qmd");
        write(&p, SAMPLE);
        let c = parse_qmd(&p).unwrap();
        assert_eq!(c.frontmatter.uuid, "abc-123");
        assert_eq!(c.frontmatter.name.as_deref(), Some("Treemap layout"));
        assert_eq!(c.frontmatter.account_uuid.as_deref(), Some("acct-1"));
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].sender, "Human");
        assert_eq!(c.messages[0].when.as_deref(), Some("2025-04-10 14:00:00"));
        assert!(c.messages[0].text.contains("treemap"));
        assert!(c.messages[1].text.contains("squarified"));
        assert!(c.messages[1].text.contains("Multiple paragraphs"));
    }

    #[test]
    fn scan_finds_qmd_under_account_dirs() {
        let root = tempdir();
        let p = root
            .join("anthropic")
            .join("acct-1")
            .join("llm_chats")
            .join("a.qmd");
        write(&p, SAMPLE);
        let other = root
            .join("anthropic")
            .join("acct-1")
            .join("llm_chats")
            .join("notes.txt");
        write(&other, "ignore");
        let scanned = scan_root(&root);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].ends_with("a.qmd"));
    }

    #[test]
    fn scan_missing_root_returns_empty() {
        let root = tempdir().join("does-not-exist");
        assert!(scan_root(&root).is_empty());
    }

    #[test]
    fn missing_frontmatter_errors() {
        let dir = tempdir();
        let p = dir.join("bad.qmd");
        write(&p, "no frontmatter here\n");
        assert!(parse_qmd(&p).is_err());
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "fw-qmd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }
}
