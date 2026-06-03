//! F1: Config loader for `~/.config/frankweiler/config.yaml`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_root: PathBuf,
    #[serde(default)]
    pub qmd: QmdConfig,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub dolt: DoltConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

/// Settings for `frankweiler-sync` — the one-shot pipeline that walks
/// every enabled source's Extract → Translate → Load chain. Outputs land
/// directly under `Config.data_root` in fixed subdirs (`rendered_md/`,
/// `dolt_db/`, `qmd/`), so there's no `out:` knob anymore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Run extract AND translate for all enabled sources concurrently.
    /// The translate phase shares a WAL-mode sqlx pool against the
    /// index doltlite, so per-doc writes serialize at the SQLite level
    /// but task scheduling stays non-blocking.
    #[serde(default = "default_true")]
    pub parallel: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { parallel: true }
    }
}

// ---------------------------------------------------------------------------
// Sources: one `type:` discriminator. `type` collapses what used to be three
// fields (`provider`, `kind`, `provenance`) into one — think of `type:` as
// the name of a constructor and the rest of the source dict as its arguments.
// Mirrors `SourceConfig` in `src/ingest/config.py`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCommon {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub input_path: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClaudeApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub overlap: Option<i64>,
    /// When non-empty, restrict the fetch to exactly these conversation
    /// UUIDs. Accepts either the bare UUID or a paste-able browser URL
    /// (`https://claude.ai/chat/<uuid>`); URLs are normalized to the
    /// trailing path segment. Skips org listing entirely; each UUID is
    /// looked up across all orgs the account has access to.
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChatgptApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_pages: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub sleep_between: Option<f64>,
    /// When non-empty, restrict the fetch to exactly these conversation
    /// IDs. Accepts either the bare id or a paste-able browser URL
    /// (`https://chatgpt.com/c/<id>`); URLs are normalized to the
    /// trailing path segment. Skips paginated listing entirely;
    /// `me.json` is still fetched.
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SlackApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub all_channels: bool,
    #[serde(default = "default_true")]
    pub media: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_prs: Option<i64>,
    /// Explicit PR refs to fetch. Each entry is a paste-able reference
    /// — either `owner/repo#NUM`, `owner/repo/pull/NUM`, or a full
    /// github.com PR URL. When non-empty, discovery is skipped and only
    /// these PRs are fetched; mirrors the `conv_uuids` shape used by
    /// the other providers so URLs paste straight in from the browser.
    #[serde(default)]
    pub pull_requests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GitlabApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_mrs: Option<i64>,
    /// Explicit MR refs to fetch. Each entry is a paste-able reference
    /// — either `namespace/project!IID` or a gitlab.com MR URL. When
    /// non-empty, discovery is skipped and only these MRs are fetched.
    #[serde(default)]
    pub merge_requests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotionInbox {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub types: Option<Vec<String>>,
    #[serde(default)]
    pub notification_page_size: Option<i64>,
    #[serde(default)]
    pub max_notification_pages: Option<i64>,
    #[serde(default)]
    pub space: Option<String>,
    /// When `false`, walk the inbox to discover referenced page IDs (and
    /// log them) but don't BFS into them. Useful for keeping the inbox
    /// signal without dragging hundreds of unrelated pages through the
    /// mirror. Defaults to `true` for back-compat.
    #[serde(default)]
    pub mirror_referenced_pages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotionSubtrees {
    /// Page IDs at the root of each subtree to walk. Accepts bare page
    /// IDs (dashed or undashed) or paste-able browser URLs
    /// (`https://www.notion.so/<workspace>/<title>-<hex32>`); URLs are
    /// reduced to the trailing 32-hex token before being passed through
    /// `format_uuid` in the notion extractor.
    #[serde(default)]
    pub pages: Vec<String>,
    #[serde(default)]
    pub max_pages: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BeeperSync {
    /// Canonical chat network names to ingest (`"signal"`,
    /// `"googlechat"`, future: `"slack"`, `"whatsapp"`, …). Empty
    /// list is an error at fetch time — caller should pick at least
    /// one explicitly.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Override for Beeper Texts' data dir. Defaults to
    /// `~/Library/Application Support/BeeperTexts` on macOS.
    #[serde(default)]
    pub beeper_data_dir: Option<PathBuf>,
    /// Copy cached media bytes into the `blobs` table. Off = metadata
    /// + source URL only.
    #[serde(default = "default_true")]
    pub media: bool,
    /// Period each rendered markdown document covers. One of
    /// `"month"` (default), `"day"`, `"year"`, or `"all"` (single
    /// file per conversation). Reactions render in the period of
    /// the message they target, regardless of when the reaction
    /// itself landed.
    #[serde(default)]
    pub period: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotionApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub inbox: Option<NotionInbox>,
    #[serde(default)]
    pub subtrees: Option<NotionSubtrees>,
}

/// Discriminated union over the literal `type:` field. Variant payloads
/// flatten the common (name/enabled/input_path) fields so the YAML shape
/// matches the Python pydantic models byte-for-byte.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    ClaudeExport {
        #[serde(flatten)]
        common: SourceCommon,
    },
    ClaudeApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<ClaudeApiSync>,
    },
    ChatgptApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<ChatgptApiSync>,
    },
    SlackApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<SlackApiSync>,
    },
    GithubApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<GithubApiSync>,
    },
    GitlabApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<GitlabApiSync>,
    },
    NotionApi {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<NotionApiSync>,
    },
    Beeper {
        #[serde(flatten)]
        common: SourceCommon,
        #[serde(default)]
        sync: Option<BeeperSync>,
    },
}

impl SourceConfig {
    pub fn common(&self) -> &SourceCommon {
        match self {
            SourceConfig::ClaudeExport { common }
            | SourceConfig::ClaudeApi { common, .. }
            | SourceConfig::ChatgptApi { common, .. }
            | SourceConfig::SlackApi { common, .. }
            | SourceConfig::GithubApi { common, .. }
            | SourceConfig::GitlabApi { common, .. }
            | SourceConfig::NotionApi { common, .. }
            | SourceConfig::Beeper { common, .. } => common,
        }
    }

    pub fn name(&self) -> &str {
        &self.common().name
    }

    pub fn enabled(&self) -> bool {
        self.common().enabled
    }

    /// Wire-format discriminator value (`"slack_api"`, `"claude_export"`, …).
    /// Matches the `type:` value in YAML.
    pub fn type_str(&self) -> &'static str {
        match self {
            SourceConfig::ClaudeExport { .. } => "claude_export",
            SourceConfig::ClaudeApi { .. } => "claude_api",
            SourceConfig::ChatgptApi { .. } => "chatgpt_api",
            SourceConfig::SlackApi { .. } => "slack_api",
            SourceConfig::GithubApi { .. } => "github_api",
            SourceConfig::GitlabApi { .. } => "gitlab_api",
            SourceConfig::NotionApi { .. } => "notion_api",
            SourceConfig::Beeper { .. } => "beeper",
        }
    }

    /// True when this source has a `sync:` block — i.e. the worker is
    /// allowed to download into it.
    pub fn is_managed(&self) -> bool {
        match self {
            SourceConfig::ClaudeExport { .. } => false,
            SourceConfig::ClaudeApi { sync, .. } => sync.is_some(),
            SourceConfig::ChatgptApi { sync, .. } => sync.is_some(),
            SourceConfig::SlackApi { sync, .. } => sync.is_some(),
            SourceConfig::GithubApi { sync, .. } => sync.is_some(),
            SourceConfig::GitlabApi { sync, .. } => sync.is_some(),
            SourceConfig::NotionApi { sync, .. } => sync.is_some(),
            SourceConfig::Beeper { sync, .. } => sync.is_some(),
        }
    }

    /// Resolved on-disk input directory: the explicit `input_path:` if set,
    /// else `<data_root>/raw/<name>`. Matches `_fill_input_path_defaults`
    /// in `src/ingest/config.py`.
    pub fn resolved_input_path(&self, data_root: &Path) -> PathBuf {
        if let Some(p) = &self.common().input_path {
            expand_tilde(&p.display().to_string())
        } else {
            data_root.join("raw").join(self.name())
        }
    }
}

/// Settings for the single doltlite file the backend reads/writes.
///
/// doltlite is a SQLite fork; the SQL store is just a file on disk,
/// `<Config.data_root>/<dolt.db_filename>`. No subprocess, no TCP port,
/// no auth — the file system is the access boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoltConfig {
    /// Filename of the doltlite database, relative to `Config.data_root`.
    /// Defaults to `backend_index.doltlite_db`.
    #[serde(default = "default_dolt_db_filename")]
    pub db_filename: String,
}

fn default_dolt_db_filename() -> String {
    "backend_index.doltlite_db".into()
}

impl Default for DoltConfig {
    fn default() -> Self {
        Self {
            db_filename: default_dolt_db_filename(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QmdConfig {
    /// Path to the qmd index file. `${data_root}` is expanded against
    /// `Config.data_root` after load. Defaults to the canonical location the
    /// `frankweiler-qmd-indexer` writes to.
    #[serde(default = "default_qmd_index_path")]
    pub index_path: String,
    /// npm package version of `@tobilu/qmd` to invoke via `npx`. Must
    /// match the version the indexer wrote with — the on-disk SQLite
    /// schema isn't versioned in a way the runner can detect.
    #[serde(default = "default_qmd_version")]
    pub qmd_version: String,
    /// qmd collection name passed to `qmd collection add` at index time;
    /// also forms the `qmd://<collection>/…` URIs the runner reads back.
    #[serde(default = "default_qmd_collection")]
    pub collection: String,
    /// Skip building the qmd index during `frankweiler-sync`. Useful in
    /// CI environments without Node.js, or when iterating on the ETL
    /// pipeline and the embedding step is too slow.
    #[serde(default)]
    pub skip: bool,
    /// Directory where `qmd` should cache its ~300MB embedding model.
    /// Defaults to `~/.cache/qmd/models` (matching qmd's own default),
    /// so a standalone `qmd` run and the sync runner share one cache.
    /// The sync runner symlinks this into its scratch workspace so the
    /// model blob stays outside the data root.
    #[serde(default)]
    pub models_dir: Option<PathBuf>,
}

impl Default for QmdConfig {
    fn default() -> Self {
        Self {
            index_path: default_qmd_index_path(),
            qmd_version: default_qmd_version(),
            collection: default_qmd_collection(),
            skip: false,
            models_dir: None,
        }
    }
}

fn default_qmd_index_path() -> String {
    format!("${{data_root}}/{}", crate::qmd::QMD_INDEX_REL)
}
fn default_qmd_version() -> String {
    crate::qmd::DEFAULT_QMD_VERSION.into()
}
fn default_qmd_collection() -> String {
    crate::qmd::DEFAULT_COLLECTION.into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub bind: String,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8731".into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("duplicate source names: {0:?}")]
    DuplicateSourceNames(Vec<String>),
    #[error(
        "notion_api source {0:?} sync: must enable inbox or list at least one \
         subtree page (set `inbox.enabled: true` and/or `subtrees.pages: [...]`)"
    )]
    NotionSyncEmpty(String),
    #[error("source name must be non-empty")]
    EmptySourceName,
}

impl Config {
    /// Resolve `${data_root}` and `~` in derived paths after load.
    pub fn resolved_qmd_index(&self) -> PathBuf {
        let s = self
            .qmd
            .index_path
            .replace("${data_root}", &self.data_root.display().to_string());
        expand_tilde(&s)
    }

    /// Absolute path to the rendered-markdown tree.
    pub fn rendered_md_path(&self) -> PathBuf {
        self.data_root.join("rendered_md")
    }

    /// Validate cross-source invariants: non-empty names, unique names, and
    /// per-source sync constraints (currently just Notion). Called by
    /// `load_config` after deserialize.
    fn validate(&self) -> Result<(), ConfigError> {
        let mut names: Vec<&str> = Vec::with_capacity(self.sources.len());
        for s in &self.sources {
            let name = s.name();
            if name.trim().is_empty() {
                return Err(ConfigError::EmptySourceName);
            }
            if let SourceConfig::NotionApi {
                sync: Some(sync), ..
            } = s
            {
                let inbox_on = sync.inbox.as_ref().is_some_and(|i| i.enabled);
                let subtrees_on = sync.subtrees.as_ref().is_some_and(|t| !t.pages.is_empty());
                if !inbox_on && !subtrees_on {
                    return Err(ConfigError::NotionSyncEmpty(name.into()));
                }
            }
            names.push(name);
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let dupes: Vec<String> = sorted
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0].to_string())
            .collect();
        if !dupes.is_empty() {
            let mut d = dupes;
            d.dedup();
            return Err(ConfigError::DuplicateSourceNames(d));
        }
        Ok(())
    }

    /// Sources with `enabled: true` (default). Mirrors `Config.enabled_sources`
    /// in `src/ingest/config.py`.
    pub fn enabled_sources(&self) -> impl Iterator<Item = &SourceConfig> {
        self.sources.iter().filter(|s| s.enabled())
    }

    /// Absolute path to the single doltlite file this backend reads/writes.
    ///
    /// Resolves to `<root>/<dolt.db_filename>`.
    pub fn dolt_db_path(&self) -> PathBuf {
        self.data_root.join(&self.dolt.db_filename)
    }
}

pub fn default_config_path() -> PathBuf {
    if let Ok(env) = std::env::var("FRANKWEILER_CONFIG") {
        return PathBuf::from(env);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/frankweiler/config.yaml");
    }
    PathBuf::from("config.yaml")
}

pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    let owned;
    let p = match path {
        Some(p) => p,
        None => {
            owned = default_config_path();
            owned.as_path()
        }
    };
    if !p.exists() {
        return Err(ConfigError::NotFound(p.to_path_buf()));
    }
    let raw = std::fs::read_to_string(p)?;
    let mut cfg: Config = serde_yaml::from_str(&raw)?;
    cfg.data_root = expand_tilde(&cfg.data_root.display().to_string());
    cfg.validate()?;
    Ok(cfg)
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_minimal_config() {
        let tmp = tempdir();
        let root = tmp.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = tmp.join("config.yaml");
        std::fs::write(&cfg_path, format!("data_root: {}\n", root.display())).unwrap();
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.data_root, root);
        assert_eq!(cfg.backend.bind, "127.0.0.1:8731");
    }

    #[test]
    fn resolves_qmd_template() {
        let tmp = tempdir();
        let cfg = Config {
            data_root: tmp.clone(),
            qmd: QmdConfig::default(),
            backend: BackendConfig::default(),
            dolt: DoltConfig::default(),
            sync: SyncConfig::default(),
            sources: Vec::new(),
        };
        let resolved = cfg.resolved_qmd_index();
        assert!(resolved.starts_with(&tmp));
        assert!(resolved.ends_with("index.sqlite"));
    }

    #[test]
    fn dolt_defaults() {
        let cfg = DoltConfig::default();
        assert_eq!(cfg.db_filename, "backend_index.doltlite_db");
    }

    #[test]
    fn dolt_db_path_default() {
        let tmp = tempdir();
        let cfg = Config {
            data_root: tmp.clone(),
            qmd: QmdConfig::default(),
            backend: BackendConfig::default(),
            dolt: DoltConfig::default(),
            sync: SyncConfig::default(),
            sources: Vec::new(),
        };
        assert_eq!(cfg.dolt_db_path(), tmp.join("backend_index.doltlite_db"));
    }

    #[test]
    fn loads_dolt_block_from_yaml() {
        let tmp = tempdir();
        let root = tmp.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = tmp.join("config.yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "data_root: {}\ndolt:\n  db_filename: my.db\n",
                root.display()
            ),
        )
        .unwrap();
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.dolt.db_filename, "my.db");
        assert_eq!(cfg.dolt_db_path(), root.join("my.db"));
    }

    fn write_cfg(yaml: &str) -> (PathBuf, PathBuf) {
        let tmp = tempdir();
        let root = tmp.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = tmp.join("config.yaml");
        let body = yaml.replace("__ROOT__", &root.display().to_string());
        std::fs::write(&cfg_path, body).unwrap();
        (cfg_path, root)
    }

    #[test]
    fn loads_one_of_each_source_type() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: claude-export
    type: claude_export
  - name: claude-api
    type: claude_api
    sync: {refresh_window_days: 14, overlap: 2}
  - name: chatgpt
    type: chatgpt_api
    sync: {max_pages: 5}
  - name: slack
    type: slack_api
    sync: {channels: ['c1','c2'], media: false}
  - name: gh
    type: github_api
    sync: {max_prs: 50}
  - name: gl
    type: gitlab_api
    sync: {max_mrs: 50}
  - name: notion
    type: notion_api
    sync:
      inbox: {enabled: true}
      subtrees: {pages: ['p1']}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.sources.len(), 7);
        assert_eq!(cfg.sources[0].type_str(), "claude_export");
        assert!(!cfg.sources[0].is_managed());
        let slack = cfg
            .sources
            .iter()
            .find(|s| s.name() == "slack")
            .expect("slack source");
        assert!(slack.is_managed());
        if let SourceConfig::SlackApi { sync, .. } = slack {
            let sync = sync.as_ref().unwrap();
            assert_eq!(
                sync.channels.as_deref(),
                Some(&["c1".to_string(), "c2".to_string()][..])
            );
            assert!(!sync.media);
        } else {
            panic!("expected SlackApi");
        }
    }

    #[test]
    fn rejects_duplicate_source_names() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - {name: dup, type: claude_export}
  - {name: dup, type: claude_export}
",
        );
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::DuplicateSourceNames(_))
        ));
    }

    #[test]
    fn rejects_notion_sync_without_inbox_or_subtrees() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: n
    type: notion_api
    sync:
      inbox: {enabled: false}
",
        );
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::NotionSyncEmpty(_))
        ));
    }

    #[test]
    fn input_path_defaults_under_data_root() {
        let (cfg_path, root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: slack
    type: slack_api
    sync: {channels: ['c']}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let s = &cfg.sources[0];
        assert_eq!(
            s.resolved_input_path(&cfg.data_root),
            root.join("raw/slack")
        );
    }

    #[test]
    fn enabled_sources_filters_disabled() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - {name: on, type: claude_export}
  - {name: off, type: claude_export, enabled: false}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let names: Vec<&str> = cfg.enabled_sources().map(|s| s.name()).collect();
        assert_eq!(names, vec!["on"]);
    }

    /// Pytest-tmp_path-style: every call yields a brand-new, uniquely-named
    /// directory under the OS temp area. We use `tempfile::TempDir` for the
    /// uniqueness guarantee (mkdtemp under the hood) and detach it with
    /// `.into_path()` so the caller can return a `PathBuf` and tests can
    /// run in parallel without colliding on a shared name.
    fn tempdir() -> PathBuf {
        tempfile::TempDir::with_prefix("fw-cfg-")
            .expect("create tempdir")
            .keep()
    }
}
