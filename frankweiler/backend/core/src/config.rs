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
        let s = self.qmd.index_path.replace("${root}", &self.root.display().to_string());
        expand_tilde(&s)
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
        std::fs::write(
            &cfg_path,
            format!("root: {}\n", root.display()),
        )
        .unwrap();
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
        };
        let resolved = cfg.resolved_qmd_index();
        assert!(resolved.starts_with(&tmp));
        assert!(resolved.ends_with("qmd-index"));
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
