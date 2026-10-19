//! Where pinned fetches are kept: `$XDG_CACHE_HOME/datalib`, else
//! `~/.cache/datalib`, the same on every platform, and shared by every
//! data root on this host (Node runtimes, model files). The fingerprint
//! cache has its own rule (`datalib_etl_files::fingerprint_cache`).

use std::path::PathBuf;

/// `None` when neither `$XDG_CACHE_HOME` nor `$HOME` is set.
pub fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("datalib"))
}
