//! How this repo spawns the `qmd` CLI, and the one version pin that says
//! which `qmd` that is.

use std::path::{Path, PathBuf};

/// The ONE canonical qmd version pin. Every spawn site — the indexer,
/// the search runner, and the daemon — runs exactly this version, and
/// `//tools:version_pins_test` asserts the fixture/Docker pins agree.
/// `datalib/tauri/stage-runtime.sh` greps this constant to decide which
/// qmd tree to bundle — keep the `DEFAULT_QMD_VERSION` name and
/// string-literal shape.
pub const DEFAULT_QMD_VERSION: &str = "2.8.3";

/// Canonical sub-path of the qmd index, relative to `<root>`. qmd writes
/// `qmd/index.sqlite` under whatever `XDG_CACHE_HOME` it is given, and
/// it is given the `qmd_index` step's own tree (see [`qmd_cache_home`]),
/// so the step writes only the tree its id names.
pub const QMD_INDEX_REL: &str = "unified_index/qmd_index/qmd/index.sqlite";

/// The `XDG_CACHE_HOME` the qmd CLI runs with for a data root: the
/// `qmd_index` step's tree, `<root>/unified_index/qmd_index`.
pub fn qmd_cache_home(root: &Path) -> PathBuf {
    crate::layout::qmd_dir(root)
}

/// Where qmd keeps its state under [`qmd_cache_home`]: the index, and the
/// `models` symlink the indexer maintains. qmd fixes the `qmd/` segment.
pub fn qmd_state_dir(root: &Path) -> PathBuf {
    qmd_cache_home(root).join("qmd")
}

pub fn qmd_index_path(root: &Path) -> PathBuf {
    qmd_state_dir(root).join("index.sqlite")
}

/// Entry script of the `@tobilu/qmd` package inside a staged runtime
/// tree — what the package's `bin/qmd` launcher execs (see
/// `third-party/qmd/bin/qmd`), so running it via node directly is
/// equivalent to `npx -y @tobilu/qmd@<v>`.
const QMD_ENTRY_REL: &str = "node_modules/@tobilu/qmd/dist/cli/qmd.js";

/// `Command` invoking the qmd CLI at exactly `version`: the app-bundled
/// Node runtime when that version is staged (see [`crate::node_runtime`]),
/// else `npx -y @tobilu/qmd@<version>`. Every qmd shell-out (indexer,
/// runner, daemon) must go through this so the bundled/npx choice stays
/// in one place.
pub fn qmd_command(version: &str) -> std::process::Command {
    crate::node_runtime::bundled_command("qmd", version, QMD_ENTRY_REL)
        .unwrap_or_else(|| crate::node_runtime::npx_command(&format!("@tobilu/qmd@{version}")))
}
