//! The CUDA module a release tarball names in `runtime.manifest`,
//! fetched into `<cache>/cuda/<sha12>/lib/libggml-cuda.so` on first use
//! (laid out as [`crate::cached_asset`] says).
//! `datalib_runtime::cuda_module` decides *when* this runs: no module
//! beside the binary, and an NVIDIA driver on the machine.

use std::path::{Path, PathBuf};

use anyhow::Result;
use datalib_runtime::cuda_module::{enable_fetch, fetched_module, CudaFetcher};
use datalib_runtime::node_runtime::RuntimeAsset;

const TAG: &str = "[cuda]";

/// The fetcher `datalib_runtime::cuda_module` calls on a miss. One per
/// process, set through [`enable_cuda_fetch`].
pub struct ReleaseCudaFetcher;

impl CudaFetcher for ReleaseCudaFetcher {
    fn fetch(&self, asset: &RuntimeAsset, cache_dir: &Path) -> Result<PathBuf, String> {
        // On its own thread, as the runtime's: the embedder can first ask
        // from an async runtime's thread, where reqwest's blocking client
        // refuses to be dropped.
        let asset = asset.clone();
        let cache_dir = cache_dir.to_path_buf();
        std::thread::spawn(move || ensure_cuda_module(&asset, &cache_dir))
            .join()
            .map_err(|_| "the fetch thread panicked".to_string())?
            .map_err(|e| format!("{e:#}"))
    }
}

/// Let this process fetch the CUDA module its manifest names, where
/// there is a GPU and none beside the binary. Called once, early, by
/// `datalib-step`, the binary that embeds.
pub fn enable_cuda_fetch() {
    enable_fetch(Box::new(ReleaseCudaFetcher));
}

/// The module `asset` holds, in place under `cache_dir`: present already,
/// or fetched, verified and unpacked now. Returns the module's path.
pub fn ensure_cuda_module(asset: &RuntimeAsset, cache_dir: &Path) -> Result<PathBuf> {
    crate::cached_asset::ensure(cache_dir, asset, TAG, "the CUDA module", || Ok(()))?;
    let module = fetched_module(cache_dir, asset);
    anyhow::ensure!(
        module.is_file(),
        "{} unpacked without {}",
        asset.name,
        module.display()
    );
    Ok(module)
}
