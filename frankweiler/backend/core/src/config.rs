//! F1: Config loader for `~/.config/frankweiler/config.yaml`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub root: PathBuf,
    #[serde(default)]
    pub qmd: QmdConfig,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub dolt: DoltConfig,
}

/// Settings for the managed `dolt sql-server` the backend talks to at
/// runtime. Mirrors the shape of `DoltConfig` in `src/ingest/config.py` so
/// the same `~/.config/personal-mirror/config.yaml` `dolt:` block can drive
/// both ingest and the Rust backend.
///
/// `repo_dirname` is the directory under `Config.root` that holds the Dolt
/// repository; defaults to `"dolt_repo"`, matching `DOLT_REPO_DIRNAME` in
/// `src/ingest/dolt_service.py`.
///
/// `binary` is an optional override for the `dolt` executable; `None` means
/// look up `dolt` on `$PATH`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoltConfig {
    #[serde(default = "default_dolt_host")]
    pub host: String,
    #[serde(default = "default_dolt_port")]
    pub port: u16,
    #[serde(default = "default_dolt_user")]
    pub user: String,
    #[serde(default = "default_dolt_repo_dirname")]
    pub repo_dirname: String,
    #[serde(default)]
    pub binary: Option<PathBuf>,
}

fn default_dolt_host() -> String {
    "127.0.0.1".into()
}
fn default_dolt_port() -> u16 {
    3306
}
fn default_dolt_user() -> String {
    "root".into()
}
fn default_dolt_repo_dirname() -> String {
    "dolt_repo".into()
}

impl Default for DoltConfig {
    fn default() -> Self {
        Self {
            host: default_dolt_host(),
            port: default_dolt_port(),
            user: default_dolt_user(),
            repo_dirname: default_dolt_repo_dirname(),
            binary: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QmdConfig {
    /// `${root}` is expanded against `Config.root` after load.
    pub index_path: String,
}

impl Default for QmdConfig {
    fn default() -> Self {
        Self {
            index_path: "${root}/.frankweiler/qmd-index".into(),
        }
    }
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
    #[error("root does not exist: {0}")]
    RootMissing(PathBuf),
}

impl Config {
    /// Resolve `${root}` and `~` in derived paths after load.
    pub fn resolved_qmd_index(&self) -> PathBuf {
        let s = self
            .qmd
            .index_path
            .replace("${root}", &self.root.display().to_string());
        expand_tilde(&s)
    }

    /// Absolute path to the Dolt repository this backend reads/writes.
    ///
    /// Resolves to `<root>/<dolt.repo_dirname>`. Matches the layout
    /// established by `DoltService` in `src/ingest/dolt_service.py`.
    pub fn dolt_repo_path(&self) -> PathBuf {
        self.root.join(&self.dolt.repo_dirname)
    }

    /// MySQL connection URL for the running `dolt sql-server`. The database
    /// name is the repo directory name (Dolt's default).
    pub fn dolt_mysql_url(&self) -> String {
        format!(
            "mysql://{user}@{host}:{port}/{db}",
            user = self.dolt.user,
            host = self.dolt.host,
            port = self.dolt.port,
            db = self.dolt.repo_dirname,
        )
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
    cfg.root = expand_tilde(&cfg.root.display().to_string());
    if !cfg.root.exists() {
        return Err(ConfigError::RootMissing(cfg.root.clone()));
    }
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
        std::fs::write(&cfg_path, format!("root: {}\n", root.display())).unwrap();
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.root, root);
        assert_eq!(cfg.backend.bind, "127.0.0.1:8731");
    }

    #[test]
    fn errors_on_missing_root() {
        let tmp = tempdir();
        let cfg_path = tmp.join("config.yaml");
        std::fs::write(&cfg_path, "root: /no/such/path\n").unwrap();
        assert!(matches!(
            load_config(Some(&cfg_path)),
            Err(ConfigError::RootMissing(_))
        ));
    }

    #[test]
    fn resolves_qmd_template() {
        let tmp = tempdir();
        let cfg = Config {
            root: tmp.clone(),
            qmd: QmdConfig::default(),
            backend: BackendConfig::default(),
            dolt: DoltConfig::default(),
        };
        let resolved = cfg.resolved_qmd_index();
        assert!(resolved.starts_with(&tmp));
        assert!(resolved.ends_with("qmd-index"));
    }

    #[test]
    fn dolt_defaults_match_python_ingest() {
        // Defaults must stay aligned with `DoltConfig` in
        // `src/ingest/config.py` so a single yaml drives both.
        let cfg = DoltConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 3306);
        assert_eq!(cfg.user, "root");
        assert_eq!(cfg.repo_dirname, "dolt_repo");
        assert!(cfg.binary.is_none());
    }

    #[test]
    fn dolt_repo_path_and_url() {
        let tmp = tempdir();
        let cfg = Config {
            root: tmp.clone(),
            qmd: QmdConfig::default(),
            backend: BackendConfig::default(),
            dolt: DoltConfig::default(),
        };
        assert_eq!(cfg.dolt_repo_path(), tmp.join("dolt_repo"));
        assert_eq!(
            cfg.dolt_mysql_url(),
            "mysql://root@127.0.0.1:3306/dolt_repo"
        );
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
                "root: {}\ndolt:\n  port: 13306\n  repo_dirname: my_repo\n",
                root.display()
            ),
        )
        .unwrap();
        let cfg = load_config(Some(&cfg_path)).unwrap();
        assert_eq!(cfg.dolt.port, 13306);
        assert_eq!(cfg.dolt.repo_dirname, "my_repo");
        assert_eq!(cfg.dolt.host, "127.0.0.1");
        assert_eq!(cfg.dolt_repo_path(), root.join("my_repo"));
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fw-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
