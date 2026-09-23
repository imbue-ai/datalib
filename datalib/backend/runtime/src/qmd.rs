//! How this repo spawns the `qmd` CLI, and the one version pin that says
//! which `qmd` that is.

use std::path::{Path, PathBuf};

/// The ONE canonical qmd version pin. Every spawn site — the indexer,
/// the search runner, and the daemon — runs exactly this version, and
/// `//tools:version_pins_test` asserts the fixture/Docker pins agree.
/// `scripts/stage_runtime.sh` greps this constant to decide which
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

/// The `@tobilu/qmd` package inside a staged runtime tree.
const QMD_PKG_REL: &str = "node_modules/@tobilu/qmd";

/// Entry script of the `@tobilu/qmd` package inside a staged runtime
/// tree — what the package's `bin/qmd` launcher execs (see
/// `third-party/qmd/bin/qmd`), so running it via node directly is
/// equivalent to `npx -y @tobilu/qmd@<v>`.
const QMD_ENTRY_REL: &str = "node_modules/@tobilu/qmd/dist/cli/qmd.js";

/// The staged Node and the `@tobilu/qmd` package directory, for a caller
/// driving qmd's SDK (`dist/index.js`) from its own script instead of
/// running the CLI. `None` when qmd resolves through `npx`, which has no
/// importable path — see [`crate::node_runtime::staged_package`].
pub fn qmd_sdk_paths(version: &str) -> Option<(PathBuf, PathBuf)> {
    crate::node_runtime::staged_package("qmd", version, QMD_PKG_REL)
}

/// `Command` invoking the qmd CLI at exactly `version` from the staged
/// runtime tree (see [`crate::node_runtime::tool_command`] for the gated
/// `npx` fallback). Every qmd shell-out (indexer, runner, daemon) must
/// go through this so the resolution stays in one place.
pub fn qmd_command(
    version: &str,
) -> Result<std::process::Command, crate::node_runtime::MissingRuntime> {
    crate::node_runtime::tool_command(
        "qmd",
        version,
        QMD_ENTRY_REL,
        &format!("@tobilu/qmd@{version}"),
    )
}

/// One of qmd's GGUF models, pinned to a HuggingFace repo *revision* and
/// the sha256 of the blob at that revision — the same table
/// `third-party/qmd_models/BUILD.bazel` fetches the build's copies from
/// (`//tools:qmd_model_pins_test` holds the two equal). qmd itself would
/// pull `resolve/main/…` and trust an etag, so a re-upload upstream would
/// silently change every embedding; with the file provisioned and
/// verified here, qmd only ever finds it already in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedModel {
    pub repo: &'static str,
    pub revision: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

impl PinnedModel {
    /// The name node-llama-cpp looks for in the models dir
    /// (`hf_<owner>_<file>`, its own download naming). Anything else and
    /// it downloads its own copy beside ours.
    pub fn cache_name(&self) -> String {
        let owner = self.repo.split('/').next().unwrap_or(self.repo);
        format!("hf_{owner}_{}", self.file)
    }

    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.file
        )
    }
}

/// Every model qmd can load at this pin: embedding (every embed and
/// every query), query expansion (the applet's CLI fallback), reranker
/// (nothing today; staged so enabling it is a code change, not a
/// surprise download).
pub const PINNED_MODELS: &[PinnedModel] = &[
    PinnedModel {
        repo: "ggml-org/embeddinggemma-300M-GGUF",
        revision: "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
        file: "embeddinggemma-300M-Q8_0.gguf",
        sha256: "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63",
    },
    PinnedModel {
        repo: "tobil/qmd-query-expansion-1.7B-gguf",
        revision: "7816de0b72572c6c860ca1eddf97ba9e7fb8cc65",
        file: "qmd-query-expansion-1.7B-q4_k_m.gguf",
        sha256: "000dfb1c06efa6a049e9f64ba921c3740e2454f62abab6fa10e77bd30bb2bcc0",
    },
    PinnedModel {
        repo: "ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF",
        revision: "a02f48bb4f057028298c21fa033da2b30d7742d5",
        file: "qwen3-reranker-0.6b-q8_0.gguf",
        sha256: "22c9979ce4fbcdc5acdc310c6641c32797eff1aa980b8f7a2db8a8ea23429a48",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI entry has to sit *inside* the package directory: a caller
    /// importing the SDK from `qmd_sdk_paths` and one spawning the CLI
    /// must land in the same staged copy, or they would run two qmds.
    #[test]
    fn the_cli_entry_lives_inside_the_package_dir() {
        assert_eq!(
            QMD_ENTRY_REL,
            format!("{QMD_PKG_REL}/dist/cli/qmd.js"),
            "QMD_ENTRY_REL and QMD_PKG_REL have drifted apart"
        );
    }

    #[test]
    fn cache_names_match_node_llama_cpp_convention() {
        let names: Vec<String> = PINNED_MODELS.iter().map(|m| m.cache_name()).collect();
        assert_eq!(
            names,
            [
                "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
                "hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf",
                "hf_ggml-org_qwen3-reranker-0.6b-q8_0.gguf",
            ]
        );
        assert_eq!(
            PINNED_MODELS[0].url(),
            "https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/0f741b5a6585bd53aeb15cd1372c56f2a0f65e12/embeddinggemma-300M-Q8_0.gguf"
        );
    }
}
