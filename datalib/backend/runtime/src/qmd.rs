//! How this repo spawns the `qmd` CLI, and the one version pin that says
//! which `qmd` that is.
//!
//! This lives down here — in a crate with no dependencies at all —
//! rather than beside the search code in `datalib_unified_index`, for a
//! build-cache reason. `qmd_indexer_bin` is a `tools=` input to the
//! fixture's embedding genrule, and bazel keys an action on its tools'
//! digests, so every crate that binary links is a crate whose next edit
//! re-runs a ~90s CPU-only embed on CI. It used to link
//! `datalib_unified_index` (and through it `datalib_core`,
//! `datalib_schema`, `app_schema`, `datalib_time`) to reach three
//! constants and a `Command` builder. Now it links this.
//!
//! `datalib_unified_index::qmd` re-exports everything here, so the
//! search runner and daemon are unaffected and there is still exactly
//! one pin.

use std::path::{Path, PathBuf};

/// The ONE canonical qmd version pin. Every spawn site — the indexer,
/// the search runner, and the daemon — runs exactly this version, and
/// `//tools:version_pins_test` asserts the fixture/Docker pins agree.
/// `datalib/tauri/stage-runtime.sh` greps this constant to decide which
/// qmd tree to bundle — keep the `DEFAULT_QMD_VERSION` name and
/// string-literal shape.
///
/// History note: the runner used to carry its own same-named constant,
/// which a version bump missed (2.1.0 vs 2.5.3 for six weeks) — hence
/// one shared constant rather than a per-module default.
pub const DEFAULT_QMD_VERSION: &str = "2.8.3";

/// Canonical sub-path of the qmd index, relative to `<root>`. qmd writes
/// here when invoked with `XDG_CACHE_HOME=<root>/unified_index` (see
/// [`qmd_cache_home`]).
pub const QMD_INDEX_REL: &str = "unified_index/qmd/index.sqlite";

/// Resolve the qmd index file path under a data root.
pub fn qmd_index_path(root: &Path) -> PathBuf {
    crate::layout::qmd_dir(root).join("index.sqlite")
}

/// Resolve the `XDG_CACHE_HOME` the qmd CLI should run with for a data
/// root: `<root>/unified_index`, so qmd writes its `qmd/index.sqlite`
/// beside the grid index rather than under the server's own `system/`.
pub fn qmd_cache_home(root: &Path) -> PathBuf {
    crate::layout::unified_index_dir(root)
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
