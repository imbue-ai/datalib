//! The Node runtime a release tarball names in `runtime.manifest`,
//! fetched into the user's cache on first use (`<cache>/<sha12>/node/bin/node`,
//! laid out as [`crate::cached_asset`] says). `docs/dev/runtime_fetch.md`
//! is the map; `datalib_runtime::node_runtime` decides *when* this runs
//! (only when no `runtime/` sits beside the binaries) and where the
//! result is looked for.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use datalib_runtime::node_runtime::{enable_fetch, fetched_runtime_dir, Manifest, RuntimeFetcher};

const TAG: &str = "[runtime]";

/// The fetcher `datalib_runtime` calls on a miss. One per process, set
/// through [`enable_runtime_fetch`].
pub struct ReleaseRuntimeFetcher;

impl RuntimeFetcher for ReleaseRuntimeFetcher {
    fn fetch(&self, manifest: &Manifest, cache_dir: &Path) -> Result<PathBuf, String> {
        // On its own thread: reqwest's blocking client refuses to be
        // dropped on an async runtime's thread, and the first call can
        // come from one (`latchkey_tokio_command` in a download step).
        let manifest = manifest.clone();
        let cache_dir = cache_dir.to_path_buf();
        std::thread::spawn(move || ensure_runtime(&manifest, &cache_dir))
            .join()
            .map_err(|_| "the fetch thread panicked".to_string())?
            .map_err(|e| format!("{e:#}"))
    }
}

/// Let this process fetch the runtime its manifest names when nothing
/// bundled is found. Called once, early, by the two binaries that run
/// latchkey: `datalib-step` (a sync's downloads) and `datalib-http`
/// (browser sign-in).
pub fn enable_runtime_fetch() {
    enable_fetch(Box::new(ReleaseRuntimeFetcher));
}

/// The manifest's runtime, in place under `cache_dir`: present already,
/// or fetched, verified and unpacked now. Returns the tree's root.
pub fn ensure_runtime(manifest: &Manifest, cache_dir: &Path) -> Result<PathBuf> {
    let cpu = manifest.cpu();
    let root =
        crate::cached_asset::ensure(cache_dir, cpu, TAG, "the Node runtime", refuse_musl_host)?;
    debug_assert_eq!(root, fetched_runtime_dir(cache_dir, cpu));
    Ok(root)
}

/// The runtime asset carries a glibc Node — nodejs.org ships no musl
/// build — so a musl host (Alpine and kin, recognised by its loader)
/// gets a refusal that says so rather than an unpacked tree that cannot
/// start. The musl *binaries* tarball on a glibc host is the common
/// case and is fine.
fn refuse_musl_host() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir("/lib") else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("ld-musl-") && name.ends_with(".so.1") {
            bail!(
                "this is a musl host (/lib/{name}), and the runtime asset holds a glibc Node; \
                 install Node yourself and set DATALIB_RUNTIME_DIR, or run the docker image"
            );
        }
    }
    Ok(())
}
