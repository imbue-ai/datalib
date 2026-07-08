//! Ingest config: the orchestrator's view of `config.yaml` — the app envelope
//! ([`Config`]) plus a [`SourceConfig`] discriminated union over `type:`.
//!
//! Relocated out of `frankweiler_core::config` (Program A): it sits *above* the
//! providers (it names every source `type:`), so `http` can link the config
//! schema without pulling `core`'s db/repo/search code.
//!
//! **Compose, don't flatten (issue #41).** Each `type:` arm of [`SourceConfig`]
//! is a *newtype* over the provider's own `*-config` crate (`SlackConfig`,
//! `EmailConfig`, …), so every provider's config is defined exactly once, in its
//! crate. Each provider config *composes* a [`SourceCommon`] (`common:`) for the
//! shared per-source envelope. `name`/`enabled` stay orchestrator-owned here.
//!
//! **One mechanism: [`Config::normalize`].** All cross-node derivation — folding
//! the global `defaults:` into each source's `common`, and resolving
//! `raw_path`/`input_path` from `data_root` — happens once, eagerly, at load.
//! Downstream code receives a fully-resolved, self-contained tree and never
//! re-derives anything (there are no lazy `resolved_*` accessors).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use frankweiler_source_common::{Defaults, EventTapeConfig, ExtractParams, SourceCommon};

use frankweiler_etl_anthropic_config::AnthropicConfig;
use frankweiler_etl_beeper_config::BeeperConfig;
use frankweiler_etl_carddav_config::CarddavConfig;
use frankweiler_etl_chatgpt_config::ChatgptConfig;
use frankweiler_etl_email_config::EmailConfig;
use frankweiler_etl_fsindex_config::FsindexConfig;
use frankweiler_etl_github_config::GithubConfig;
use frankweiler_etl_gitlab_config::GitlabConfig;
use frankweiler_etl_google_takeout_config::GoogleTakeoutConfig;
use frankweiler_etl_linkedin_config::LinkedinConfig;
use frankweiler_etl_notion_config::NotionConfig;
use frankweiler_etl_perseus_config::PerseusConfig;
use frankweiler_etl_signal_config::SignalConfig;
use frankweiler_etl_slack_config::SlackConfig;
use frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreConfig;
use frankweiler_etl_whatsapp_config::WhatsappConfig;
use frankweiler_etl_yolink_config::YolinkConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Root directory for all on-disk state (`raw/`, `rendered_md/`,
    /// `dolt_db/`, `qmd/`, …). Optional in YAML: when omitted, it
    /// defaults to the directory the config file itself lives in, so a
    /// data root containing its own `config.yaml` is fully
    /// self-contained. Resolved to an absolute path by [`load_config`]
    /// (the in-memory value is never the empty-path sentinel).
    #[serde(default)]
    pub data_root: PathBuf,
    #[serde(default)]
    pub qmd: QmdConfig,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub dolt: DoltConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    /// Global base values for the propagatable per-source knobs. Pure authoring
    /// sugar: [`Config::normalize`] folds these into every source's `common` at
    /// load; DO NOT read after load — consumers use the resolved per-source
    /// `common`.
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
}

/// Settings for `frankweiler-sync` — the one-shot pipeline that walks every
/// enabled source's Extract → Translate → Load chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Run extract AND translate for all enabled sources concurrently.
    #[serde(default = "default_true")]
    pub parallel: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { parallel: true }
    }
}

// ---------------------------------------------------------------------------
// Sources. A source entry is the orchestrator-owned envelope (`name`/`enabled`)
// plus a nested `source:` discriminated union over `type:`. `type` collapses
// what used to be three fields (`provider`, `kind`, `provenance`) into one —
// think of `type:` as the name of a constructor and the rest of `source:` as
// its arguments. Mirrors `SourceConfig` in `src/ingest/config.py`.
// ---------------------------------------------------------------------------

/// One entry of `sources:`. The orchestrator owns `name` (identity in the list)
/// and `enabled` (run-or-not); everything the provider needs lives under
/// `source:`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub source: SourceConfig,
}

impl SourceEntry {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn type_str(&self) -> &'static str {
        self.source.type_str()
    }
    pub fn is_managed(&self) -> bool {
        self.source.is_managed()
    }
    /// Resolved raw-store directory (valid after [`Config::normalize`]).
    pub fn raw_path(&self) -> &Path {
        self.source.common().raw_path()
    }
    /// Resolved input path: explicit `input_path` else the raw dir (valid after
    /// [`Config::normalize`]).
    pub fn input_path(&self) -> &Path {
        self.source.common().input_or_raw_path()
    }
}

/// Discriminated union over the literal `type:` field. Each arm is a *newtype*
/// over the provider's own `*-config` type, so provider config is defined once.
/// serde reads `type:`, strips it, and deserializes the remaining keys into the
/// inner config (which composes `common:` for the shared envelope). No
/// `flatten` anywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    ClaudeExport(AnthropicConfig),
    ClaudeApi(AnthropicConfig),
    ChatgptApi(ChatgptConfig),
    SlackApi(SlackConfig),
    GithubApi(GithubConfig),
    GitlabApi(GitlabConfig),
    NotionApi(NotionConfig),
    Email(EmailConfig),
    Beeper(BeeperConfig),
    Carddav(CarddavConfig),
    Linkedin(LinkedinConfig),
    GoogleTakeout(GoogleTakeoutConfig),
    Perseus(PerseusConfig),
    Yolink(YolinkConfig),
    SignalBackup(SignalConfig),
    /// Explicit `rename` overrides serde's `snake_case`, which would otherwise
    /// derive `whats_app_backup` from the two capitalized segments of
    /// `WhatsAppBackup`.
    #[serde(rename = "whatsapp_backup")]
    WhatsAppBackup(WhatsappConfig),
    SmsBackupRestore(SmsBackupRestoreConfig),
    /// Directory-tree scanner (Unison-style fast rescan). File-backed and
    /// **extract-only** — it indexes the tree at `input_path` into a raw store
    /// and renders nothing.
    Fsindex(FsindexConfig),
}

/// Dispatch an expression over the payload of any variant, binding it to `$c`.
/// Works because every payload composes a `common: SourceCommon` field and
/// exposes `validate()`.
macro_rules! over_payload {
    ($self:expr, $c:ident => $e:expr) => {
        match $self {
            SourceConfig::ClaudeExport($c) | SourceConfig::ClaudeApi($c) => $e,
            SourceConfig::ChatgptApi($c) => $e,
            SourceConfig::SlackApi($c) => $e,
            SourceConfig::GithubApi($c) => $e,
            SourceConfig::GitlabApi($c) => $e,
            SourceConfig::NotionApi($c) => $e,
            SourceConfig::Email($c) => $e,
            SourceConfig::Beeper($c) => $e,
            SourceConfig::Carddav($c) => $e,
            SourceConfig::Linkedin($c) => $e,
            SourceConfig::GoogleTakeout($c) => $e,
            SourceConfig::Perseus($c) => $e,
            SourceConfig::Yolink($c) => $e,
            SourceConfig::SignalBackup($c) => $e,
            SourceConfig::WhatsAppBackup($c) => $e,
            SourceConfig::SmsBackupRestore($c) => $e,
            SourceConfig::Fsindex($c) => $e,
        }
    };
}

impl SourceConfig {
    /// The shared per-source envelope (`common:`).
    pub fn common(&self) -> &SourceCommon {
        over_payload!(self, c => &c.common)
    }

    /// Mutable access to the envelope — used by [`Config::normalize`].
    pub fn common_mut(&mut self) -> &mut SourceCommon {
        over_payload!(self, c => &mut c.common)
    }

    /// Provider-local validation, delegated to the owning `*-config` crate.
    pub fn validate(&self) -> anyhow::Result<()> {
        over_payload!(self, c => c.validate())
    }

    /// Wire-format discriminator value (`"slack_api"`, `"claude_export"`, …).
    pub fn type_str(&self) -> &'static str {
        match self {
            SourceConfig::ClaudeExport(_) => "claude_export",
            SourceConfig::ClaudeApi(_) => "claude_api",
            SourceConfig::ChatgptApi(_) => "chatgpt_api",
            SourceConfig::SlackApi(_) => "slack_api",
            SourceConfig::GithubApi(_) => "github_api",
            SourceConfig::GitlabApi(_) => "gitlab_api",
            SourceConfig::NotionApi(_) => "notion_api",
            SourceConfig::Email(_) => "email",
            SourceConfig::Beeper(_) => "beeper",
            SourceConfig::Carddav(_) => "carddav",
            SourceConfig::Linkedin(_) => "linkedin",
            SourceConfig::GoogleTakeout(_) => "google_takeout",
            SourceConfig::Perseus(_) => "perseus",
            SourceConfig::Yolink(_) => "yolink",
            SourceConfig::SignalBackup(_) => "signal_backup",
            SourceConfig::WhatsAppBackup(_) => "whatsapp_backup",
            SourceConfig::SmsBackupRestore(_) => "sms_backup_restore",
            SourceConfig::Fsindex(_) => "fsindex",
        }
    }

    /// True when the worker is allowed to download into / build the raw store
    /// for this source — a `sync:` block, or (for file-backed sources) an
    /// `input_path:` export on disk. `claude_export` is never managed (it is a
    /// pure translate-only view of a local export).
    pub fn is_managed(&self) -> bool {
        match self {
            SourceConfig::ClaudeExport(_) => false,
            SourceConfig::ClaudeApi(c) => c.sync.is_some(),
            SourceConfig::ChatgptApi(c) => c.sync.is_some(),
            SourceConfig::SlackApi(c) => c.sync.is_some(),
            SourceConfig::GithubApi(c) => c.sync.is_some(),
            SourceConfig::GitlabApi(c) => c.sync.is_some(),
            SourceConfig::NotionApi(c) => c.sync.is_some(),
            // Email/Carddav: `sync:` → live server; else an export at
            // `input_path` → file-backed mode. Both own the raw store.
            SourceConfig::Email(c) => c.sync.is_some() || c.common.input_path.is_some(),
            SourceConfig::Carddav(c) => c.sync.is_some() || c.common.input_path.is_some(),
            SourceConfig::Beeper(c) => c.sync.is_some(),
            // File-backed only: managed iff an `input_path:` export is set.
            SourceConfig::Linkedin(c) => c.common.input_path.is_some(),
            SourceConfig::GoogleTakeout(c) => c.common.input_path.is_some(),
            SourceConfig::Perseus(c) => c.sync.is_some(),
            SourceConfig::Yolink(c) => c.sync.is_some(),
            SourceConfig::SignalBackup(c) => c.sync.is_some(),
            SourceConfig::WhatsAppBackup(c) => c.sync.is_some(),
            SourceConfig::SmsBackupRestore(c) => c.common.input_path.is_some(),
            // File-backed only: managed iff an `input_path:` scan root is set.
            SourceConfig::Fsindex(c) => c.common.input_path.is_some(),
        }
    }
}

/// Settings for the backend index doltlite DB. Its location is canonical —
/// `data_root/system/backend_index/db.doltlite_db` (see
/// [`frankweiler_core::layout`]) — because the http server resolves it from
/// `data_root` alone and never reads this config; an override here couldn't be
/// honored on the read side, so there's nothing to configure (yet).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DoltConfig {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QmdConfig {
    /// Path to the qmd index file. `${data_root}` is expanded against
    /// `Config.data_root` after load.
    #[serde(default = "default_qmd_index_path")]
    pub index_path: String,
    /// npm package version of `@tobilu/qmd` to invoke via `npx`.
    #[serde(default = "default_qmd_version")]
    pub qmd_version: String,
    /// qmd collection name; also forms the `qmd://<collection>/…` URIs.
    #[serde(default = "default_qmd_collection")]
    pub collection: String,
    /// Skip building the qmd index during `frankweiler-sync`.
    #[serde(default)]
    pub skip: bool,
    /// Directory where `qmd` caches its embedding model. Defaults to
    /// `~/.cache/qmd/models`.
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
    format!("${{data_root}}/{}", frankweiler_core::qmd::QMD_INDEX_REL)
}
fn default_qmd_version() -> String {
    frankweiler_core::qmd::DEFAULT_QMD_VERSION.into()
}
fn default_qmd_collection() -> String {
    frankweiler_core::qmd::DEFAULT_COLLECTION.into()
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
    #[error("source name must be non-empty")]
    EmptySourceName,
    /// A source name is used verbatim as a directory component under
    /// `data_root/<name>/`, so it must be a POSIX-portable filename:
    /// `[A-Za-z0-9._-]` only, and not `.` or `..`.
    #[error("invalid source name {0:?}: {1}")]
    InvalidSourceName(String, &'static str),
    /// A provider's own `validate()` (delegated to its `*-config` crate)
    /// rejected the source.
    #[error("source {0:?}: {1}")]
    SourceInvalid(String, #[source] anyhow::Error),
}

/// A source `name` becomes a directory component (`data_root/<name>/raw`,
/// `data_root/<name>/rendered_md/…`), so it must be a portable, unambiguous
/// path segment. Accept the POSIX "portable filename character set"
/// (`[A-Za-z0-9._-]`); reject path separators, `.`/`..`, and a leading `-`
/// (which would read as a flag to CLI tools).
fn validate_source_name(name: &str) -> Result<(), &'static str> {
    if frankweiler_core::layout::RESERVED_STANZA_NAMES.contains(&name) {
        return Err("name is reserved (collides with the data_root/system directory)");
    }
    if name == "." || name == ".." {
        return Err("name must not be '.' or '..'");
    }
    if name.starts_with('-') {
        return Err("name must not start with '-'");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(match bad {
            '/' => "name must not contain '/'",
            _ => "name may only contain ASCII letters, digits, '.', '_', '-'",
        });
    }
    Ok(())
}

impl Config {
    /// The single locus of config mechanism: fold the global `defaults:` into
    /// each source's `common`, then resolve paths from `data_root`. Run once,
    /// at load, right after deserialize. Afterwards every source is fully
    /// explicit and self-contained.
    fn normalize(&mut self) {
        let data_root = self.data_root.clone();
        let defaults = self.defaults.clone();
        for entry in &mut self.sources {
            let common = entry.source.common_mut();
            common.fold_defaults(&defaults);
            common.resolve_paths(&data_root, &entry.name);
        }
    }

    /// Resolve `${data_root}` and `~` in the qmd index path after load.
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

    /// Validate cross-source invariants (non-empty + unique names) and each
    /// source's provider-local rules (delegated to its `*-config` crate).
    /// Called by [`load_config`] after [`Config::normalize`].
    fn validate(&self) -> Result<(), ConfigError> {
        let mut names: Vec<&str> = Vec::with_capacity(self.sources.len());
        for entry in &self.sources {
            let name = entry.name.trim();
            if name.is_empty() {
                return Err(ConfigError::EmptySourceName);
            }
            if let Err(reason) = validate_source_name(name) {
                return Err(ConfigError::InvalidSourceName(name.to_string(), reason));
            }
            entry
                .source
                .validate()
                .map_err(|e| ConfigError::SourceInvalid(entry.name.clone(), e))?;
            names.push(name);
        }
        names.sort_unstable();
        let mut dupes: Vec<String> = names
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0].to_string())
            .collect();
        if !dupes.is_empty() {
            dupes.dedup();
            return Err(ConfigError::DuplicateSourceNames(dupes));
        }
        Ok(())
    }

    /// Enabled sources, optionally narrowed to a single source by name.
    ///
    /// When `$FRANKWEILER_ONLY_SOURCE` is set and non-empty, only the matching
    /// source is yielded (the UI's per-source "Sync now"); unset yields every
    /// enabled source.
    pub fn enabled_sources(&self) -> impl Iterator<Item = &SourceEntry> {
        let only = std::env::var("FRANKWEILER_ONLY_SOURCE")
            .ok()
            .filter(|s| !s.is_empty());
        self.sources.iter().filter(move |e| {
            if !e.enabled {
                return false;
            }
            match only.as_deref() {
                Some(name) => e.name == name,
                None => true,
            }
        })
    }

    /// Absolute path to the backend index doltlite DB:
    /// `data_root/system/backend_index/db.doltlite_db`.
    pub fn dolt_db_path(&self) -> PathBuf {
        frankweiler_core::layout::backend_index_db(&self.data_root)
    }
}

/// Path to the config file that lives *inside* a data root: `<root>/config.yaml`.
pub fn root_config_path(data_root: &Path) -> PathBuf {
    data_root.join("config.yaml")
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
    if cfg.data_root.as_os_str().is_empty() {
        // No explicit `data_root:` — default to the directory the config
        // file itself lives in. Canonicalize first so the parent is
        // absolute even when `p` was given as a bare `config.yaml`.
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        cfg.data_root = abs
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
    } else {
        cfg.data_root = expand_tilde(&cfg.data_root.display().to_string());
    }
    cfg.normalize();
    cfg.validate()?;
    Ok(cfg)
}

fn default_true() -> bool {
    true
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
    fn data_root_defaults_to_config_dir() {
        let tmp = tempdir();
        let cfg_path = tmp.join("config.yaml");
        // No `data_root:` key at all.
        std::fs::write(&cfg_path, "sources: []\n").unwrap();
        let cfg = load_config(Some(&cfg_path)).unwrap();
        // Canonicalize the expected dir too: on macOS the temp dir is
        // under a `/var -> /private/var` symlink, which canonicalize
        // resolves.
        let want = std::fs::canonicalize(&tmp).unwrap();
        assert_eq!(cfg.data_root, want);
        // Derived paths hang off the resolved root.
        assert_eq!(
            cfg.dolt_db_path(),
            want.join("system/backend_index/db.doltlite_db")
        );
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
            defaults: Defaults::default(),
            sources: Vec::new(),
        };
        let resolved = cfg.resolved_qmd_index();
        assert!(resolved.starts_with(&tmp));
        assert!(resolved.ends_with("index.sqlite"));
    }

    #[test]
    fn dolt_db_path_is_canonical_under_system() {
        let tmp = tempdir();
        let cfg = Config {
            data_root: tmp.clone(),
            qmd: QmdConfig::default(),
            backend: BackendConfig::default(),
            dolt: DoltConfig::default(),
            sync: SyncConfig::default(),
            defaults: Defaults::default(),
            sources: Vec::new(),
        };
        assert_eq!(
            cfg.dolt_db_path(),
            tmp.join("system/backend_index/db.doltlite_db")
        );
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
    source:
      type: claude_export
  - name: claude-api
    source:
      type: claude_api
      sync: {refresh_window_days: 14, refresh_most_recent_n_chat_count: 2}
  - name: chatgpt
    source:
      type: chatgpt_api
      sync: {max_pages: 5}
  - name: slack
    source:
      type: slack_api
      sync: {channels: ['c1','c2'], media: false}
  - name: gh
    source:
      type: github_api
      sync: {max_prs: 50}
  - name: gl
    source:
      type: gitlab_api
      sync: {max_mrs: 50}
  - name: notion
    source:
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
        if let SourceConfig::SlackApi(c) = &slack.source {
            let sync = c.sync.as_ref().unwrap();
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
    fn bare_source_with_no_config_keys_parses() {
        // claude_export has no required keys: `source: {type: claude_export}`
        // must deserialize into an all-default AnthropicConfig.
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: x
    source: {type: claude_export}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.sources.len(), 1);
        assert!(!cfg.sources[0].is_managed());
    }

    #[test]
    fn rejects_unknown_type() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: x
    source: {type: not_a_provider}
",
        );
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::Yaml(_))
        ));
    }

    #[test]
    fn rejects_duplicate_source_names() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - {name: dup, source: {type: claude_export}}
  - {name: dup, source: {type: claude_export}}
",
        );
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::DuplicateSourceNames(_))
        ));
    }

    #[test]
    fn validate_source_name_rules() {
        // The reserved `system` dir name and POSIX-unsafe components are
        // rejected; ordinary names with `[A-Za-z0-9._-]` pass.
        assert!(validate_source_name("slack-work").is_ok());
        assert!(validate_source_name("github_imbue").is_ok());
        assert!(validate_source_name("v1.2").is_ok());
        assert!(validate_source_name("system").is_err()); // reserved
        assert!(validate_source_name("slack/work").is_err()); // separator
        assert!(validate_source_name(".").is_err());
        assert!(validate_source_name("..").is_err());
        assert!(validate_source_name("-leading").is_err()); // reads as a CLI flag
        assert!(validate_source_name("space name").is_err());
    }

    #[test]
    fn rejects_reserved_source_name() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - {name: system, source: {type: claude_export}}
",
        );
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::InvalidSourceName(_, _))
        ));
    }

    #[test]
    fn loads_yolink_source() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: yolink
    source:
      type: yolink
      sync:
        window_days: 7
        devices:
          - name: water_valve
            kind: watermeter
            start: '2026-04-05'
            family_device_id: '00112233445566778899aabbccddeeff'
            device_udid: 'ffeeddccbbaa99887766554433221100'
          - name: basement_freezer
            kind: temperature_humidity
            start: '2026-04-05'
            family_device_id: '0123456789abcdef0123456789abcdef'
            device_udid: 'fedcba9876543210fedcba9876543210'
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let yl = cfg.sources.iter().find(|s| s.name() == "yolink").unwrap();
        assert!(yl.is_managed());
        if let SourceConfig::Yolink(c) = &yl.source {
            let sync = c.sync.as_ref().unwrap();
            assert_eq!(sync.window_days, Some(7));
            assert_eq!(sync.devices.len(), 2);
            assert_eq!(sync.devices[0].name, "water_valve");
            assert_eq!(sync.devices[0].kind, "watermeter");
        } else {
            panic!("expected Yolink");
        }
    }

    #[test]
    fn rejects_yolink_bad_kind() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: yolink
    source:
      type: yolink
      sync:
        devices:
          - name: x
            kind: door_sensor
            start: '2026-04-05'
            family_device_id: '00112233445566778899aabbccddeeff'
            device_udid: 'ffeeddccbbaa99887766554433221100'
",
        );
        let err = load_config(Some(&cfg_path)).unwrap_err();
        assert!(matches!(err, ConfigError::SourceInvalid(_, _)));
        assert!(err.to_string().contains("unknown kind"));
    }

    #[test]
    fn rejects_yolink_bad_hex_id() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: yolink
    source:
      type: yolink
      sync:
        devices:
          - name: x
            kind: temperature_humidity
            start: '2026-04-05'
            family_device_id: 'not-hex'
            device_udid: 'ffeeddccbbaa99887766554433221100'
",
        );
        let err = load_config(Some(&cfg_path)).unwrap_err();
        assert!(matches!(err, ConfigError::SourceInvalid(_, _)));
        assert!(err.to_string().contains("family_device_id"));
    }

    #[test]
    fn rejects_yolink_empty_devices() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: yolink
    source:
      type: yolink
      sync:
        devices: []
",
        );
        let err = load_config(Some(&cfg_path)).unwrap_err();
        assert!(matches!(err, ConfigError::SourceInvalid(_, _)));
        assert!(err.to_string().contains("at least one device"));
    }

    #[test]
    fn rejects_notion_sync_without_inbox_or_subtrees() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: n
    source:
      type: notion_api
      sync:
        inbox: {enabled: false}
",
        );
        let err = load_config(Some(&cfg_path)).unwrap_err();
        assert!(matches!(err, ConfigError::SourceInvalid(_, _)));
        assert!(err.to_string().contains("inbox or list at least one"));
    }

    #[test]
    fn input_path_defaults_under_data_root() {
        let (cfg_path, root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: slack
    source:
      type: slack_api
      sync: {channels: ['c']}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        // API source: no explicit input_path → input resolves to the raw dir.
        assert_eq!(cfg.sources[0].input_path(), root.join("slack/raw"));
        assert!(cfg.sources[0].source.common().input_path.is_none());
    }

    #[test]
    fn raw_path_defaults_under_data_root_and_is_overridable() {
        let (cfg_path, root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: slack
    source:
      type: slack_api
      sync: {channels: ['c']}
  - name: gh
    source:
      type: github_api
      common:
        raw_path: /mnt/big/gh-raw
      sync: {}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        // Default: <data_root>/<name>/raw.
        assert_eq!(cfg.sources[0].raw_path(), root.join("slack/raw"));
        // Override: the store can live anywhere.
        assert_eq!(cfg.sources[1].raw_path(), PathBuf::from("/mnt/big/gh-raw"));
    }

    #[test]
    fn enabled_sources_filters_disabled() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - {name: on, source: {type: claude_export}}
  - {name: off, enabled: false, source: {type: claude_export}}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let names: Vec<&str> = cfg.enabled_sources().map(|s| s.name()).collect();
        assert_eq!(names, vec!["on"]);
    }

    #[test]
    fn defaults_fall_through_when_source_omits() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
defaults:
  blob_size_limit_bytes: 5000000
sources:
  - name: slack
    source:
      type: slack_api
      sync: {channels: ['c']}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        // After normalize(), the global default is folded into the source.
        assert_eq!(
            cfg.sources[0].source.common().blob_size_limit_bytes,
            Some(5_000_000)
        );
    }

    #[test]
    fn defaults_source_overrides_global() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
defaults:
  blob_size_limit_bytes: 5000000
sources:
  - name: slack
    source:
      type: slack_api
      common:
        blob_size_limit_bytes: 100000
      sync: {channels: ['c']}
  - name: gh
    source:
      type: github_api
      sync: {}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let slack = cfg.sources.iter().find(|s| s.name() == "slack").unwrap();
        let gh = cfg.sources.iter().find(|s| s.name() == "gh").unwrap();
        assert_eq!(slack.source.common().blob_size_limit_bytes, Some(100_000));
        // sibling still inherits the global default
        assert_eq!(gh.source.common().blob_size_limit_bytes, Some(5_000_000));
    }

    #[test]
    fn defaults_unset_means_unlimited() {
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
sources:
  - name: slack
    source:
      type: slack_api
      sync: {channels: ['c']}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.sources[0].source.common().blob_size_limit_bytes, None);
    }

    #[test]
    fn extract_params_default_when_unset() {
        let ep = ExtractParams::default();
        assert_eq!(
            ep.max_time_without_progress(),
            std::time::Duration::from_secs(30 * 60)
        );
        assert_eq!(ep.max_sequential_failures(), 50);
    }

    #[test]
    fn extract_params_source_overrides_one_field_only() {
        // Global sets both; the source overrides only the failure count. The
        // unset (minutes) field falls through to the global; a sibling inherits
        // both globals.
        let (cfg_path, _root) = write_cfg(
            "data_root: __ROOT__
defaults:
  extract_params:
    maximum_time_without_progress_in_minutes: 10
    maximum_sequential_failed_requests: 5
sources:
  - name: slack
    source:
      type: slack_api
      common:
        extract_params:
          maximum_sequential_failed_requests: 99
      sync: {channels: ['c']}
  - name: gh
    source:
      type: github_api
      sync: {}
",
        );
        let cfg = load_config(Some(&cfg_path)).unwrap();
        let slack = cfg.sources.iter().find(|s| s.name() == "slack").unwrap();
        let gh = cfg.sources.iter().find(|s| s.name() == "gh").unwrap();

        let slack_ep = &slack.source.common().extract_params;
        assert_eq!(slack_ep.max_sequential_failures(), 99);
        assert_eq!(
            slack_ep.max_time_without_progress(),
            std::time::Duration::from_secs(10 * 60)
        );

        let gh_ep = &gh.source.common().extract_params;
        assert_eq!(gh_ep.max_sequential_failures(), 5);
        assert_eq!(
            gh_ep.max_time_without_progress(),
            std::time::Duration::from_secs(10 * 60)
        );
    }

    /// Pytest-tmp_path-style: a brand-new, uniquely-named temp dir per call.
    fn tempdir() -> PathBuf {
        tempfile::TempDir::with_prefix("fw-cfg-")
            .expect("create tempdir")
            .keep()
    }
}
