//! Where the server finds the binaries it runs: `datalib-step`, for the
//! connect flow and as the directory every step's `PATH` starts with.

use std::path::PathBuf;

/// `$DATALIB_STEP_BIN` (how `dev.sh` wires it from Bazel runfiles), else
/// `datalib-step` in [`resolve_binary_dir`], else a sibling of this
/// executable.
pub fn resolve_step_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DATALIB_STEP_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        tracing::warn!("$DATALIB_STEP_BIN={} is not a file", p.display());
    }
    let dirs = resolve_binary_dir()
        .into_iter()
        .chain(own_dir())
        .collect::<Vec<_>>();
    dirs.iter()
        .flat_map(|dir| ["datalib-step", "datalib_step"].map(|name| dir.join(name)))
        .find(|cand| cand.is_file())
}

/// The step-binary directory the loop puts first on every step's `PATH`:
/// `$DATALIB_BINARY_DIR` (how `dev.sh` / `serve_dev.sh` wire a shim dir
/// from Bazel runfiles), else this executable's own directory when
/// `datalib-step` sits next to it (how a packaged release lays the
/// binaries out side by side).
pub fn resolve_binary_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DATALIB_BINARY_DIR") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
        tracing::warn!("$DATALIB_BINARY_DIR={} is not a directory", p.display());
    }
    own_dir().filter(|dir| dir.join("datalib-step").is_file())
}

fn own_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}
