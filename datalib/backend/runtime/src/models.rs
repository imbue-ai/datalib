//! The embedding model search runs, pinned; and where qmd, which search
//! used before its own index, kept its index in a data root, so
//! `unified_grid_and_search_index` can delete what an older build left there.

use std::path::{Path, PathBuf};

/// Where qmd kept its index, its config and its link to the models:
/// `<root>/unified_grid_and_search_index/qmd_aggregator/qmd`.
pub fn qmd_state_dir(root: &Path) -> PathBuf {
    crate::layout::qmd_dir(root).join("qmd")
}

/// One GGUF model, pinned to a HuggingFace repo *revision* and the sha256
/// of the blob at that revision — the same table
/// `third-party/embedding_models/BUILD.bazel` fetches the build's copies from
/// (`//tools:model_pins_test` holds the two equal), so a re-upload
/// upstream cannot change every embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedModel {
    pub repo: &'static str,
    pub revision: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

impl PinnedModel {
    /// The model's file name in the models dir: `hf_<owner>_<file>`,
    /// node-llama-cpp's download naming, under which qmd's copies are
    /// already on disk.
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

/// Every model qmd 2.8.3 could load: embedding (every embed and every
/// query), query expansion and reranker (neither loaded). Only
/// [`EMBED_MODELS`] is ever fetched or shipped.
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

/// The embedding model alone, first in [`PINNED_MODELS`]: all an embed or
/// a query loads.
pub const EMBED_MODELS: &[PinnedModel] = PINNED_MODELS.split_at(1).0;

#[cfg(test)]
mod tests {
    use super::*;

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
