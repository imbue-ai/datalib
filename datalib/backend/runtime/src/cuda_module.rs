//! Where a gnu Linux build finds llama.cpp's CUDA backend,
//! `libggml-cuda.so`: in `lib/` beside the running binary (the `-cuda`
//! image, an install that put it there), else as the release asset its
//! `runtime.manifest` names, fetched sha256-verified into the user's cache
//! on first use, and only where there is an NVIDIA driver to run it
//! (`docs/dev/runtime_fetch.md` § "The CUDA module"). The fetch lives in
//! `datalib_fetch`; a binary that wants it installs a [`CudaFetcher`]
//! through [`enable_fetch`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::node_runtime::{self, FetchOutcome, MANIFEST_FILE};
use crate::runtime_manifest::{Manifest, RuntimeAsset};

/// The module inside an install or a fetched tree.
pub const MODULE_REL: &str = "lib/libggml-cuda.so";

/// The library NVIDIA's driver installs, which the module needs and
/// nothing else on the machine provides.
pub const DRIVER_LIBRARY: &str = "libcuda.so.1";

/// `lib/libggml-cuda.so` beside `exe`, as the release tarball lays it out.
pub fn beside(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.join(MODULE_REL))
}

/// Where fetched modules live: `cuda/` in the machine cache
/// ([`crate::cache`]), apart from the runtime's, whose fetch keeps one tree
/// and removes the rest.
pub fn cache_dir() -> Option<PathBuf> {
    Some(crate::cache::cache_dir()?.join("cuda"))
}

/// The module in the tree a manifest's `cuda` asset unpacks into, named
/// by the asset's sha256 as a runtime tree is.
pub fn fetched_module(cache_dir: &Path, asset: &RuntimeAsset) -> PathBuf {
    cache_dir.join(asset.dir_name()).join(MODULE_REL)
}

/// Puts the module a manifest's `cuda` asset holds in place under
/// `cache_dir` and returns its path. Implemented by `datalib_fetch`.
pub trait CudaFetcher: Send + Sync {
    fn fetch(&self, asset: &RuntimeAsset, cache_dir: &Path) -> Result<PathBuf, String>;
}

static FETCHER: OnceLock<Box<dyn CudaFetcher>> = OnceLock::new();

/// Let a miss fetch. Called once, early, by the binary that embeds
/// (`datalib-step`); a second call is ignored.
pub fn enable_fetch(fetcher: Box<dyn CudaFetcher>) {
    let _ = FETCHER.set(fetcher);
}

/// What the machine offers, read by [`find`] and decided on by [`choose`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// `lib/libggml-cuda.so` beside the binary, and whether it is there.
    pub beside: Option<(PathBuf, bool)>,
    /// Whether NVIDIA's driver library loads.
    pub driver: bool,
    pub manifest: Result<Manifest, FetchOutcome>,
    /// The cache directory, and whether the manifest's module is in it.
    pub cache: Option<(PathBuf, bool)>,
}

/// Where the module comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A module already on disk, beside the binary or fetched before.
    Load(PathBuf),
    /// The manifest's asset, to fetch into the cache directory.
    Fetch { asset: RuntimeAsset, cache: PathBuf },
    /// No module to load, and why: what the embedder reports.
    None(String),
}

/// A module beside the binary wins; then, only with a driver to run it,
/// the manifest's, from the cache or fetched into it.
pub fn choose(seen: Seen) -> Choice {
    let Some((beside, there)) = seen.beside else {
        return Choice::None("no CUDA module: the running binary's path is unknown".into());
    };
    if there {
        return Choice::Load(beside);
    }
    let none = |why: String| Choice::None(format!("no CUDA module at {}, {why}", beside.display()));
    if !seen.driver {
        return none(format!(
            "and no NVIDIA driver ({DRIVER_LIBRARY}) to fetch one for"
        ));
    }
    let manifest = match seen.manifest {
        Ok(manifest) => manifest,
        Err(FetchOutcome::BadManifest(e)) => return none(format!("and {e}")),
        Err(_) => {
            return none(format!(
                "and no `{MANIFEST_FILE}` beside the binary names one to fetch"
            ))
        }
    };
    let Some(asset) = manifest.cuda() else {
        return none(format!("and `{MANIFEST_FILE}` names none for this build"));
    };
    match seen.cache {
        None => none(
            "and neither $XDG_CACHE_HOME nor $HOME is set, so there is no cache \
             directory to fetch it into"
                .into(),
        ),
        Some((cache, true)) => Choice::Load(fetched_module(&cache, asset)),
        Some((cache, false)) => Choice::Fetch {
            asset: asset.clone(),
            cache,
        },
    }
}

/// [`Seen`] for the running binary.
pub fn look(driver: bool) -> Seen {
    let beside = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(beside)
        .map(|path| {
            let there = path.exists();
            (path, there)
        });
    let manifest = node_runtime::manifest().cloned();
    let cache = cache_dir().map(|dir| {
        let there = manifest
            .as_ref()
            .ok()
            .and_then(Manifest::cuda)
            .is_some_and(|asset| fetched_module(&dir, asset).is_file());
        (dir, there)
    });
    Seen {
        beside,
        driver,
        manifest,
        cache,
    }
}

/// The module to load, fetching it first when [`choose`] says to, or why
/// there is none. `driver` is whether NVIDIA's driver library loads,
/// which the caller (the embedder, which can `dlopen`) knows.
pub fn find(driver: bool) -> Result<PathBuf, String> {
    match choose(look(driver)) {
        Choice::Load(path) => Ok(path),
        Choice::None(why) => Err(why),
        Choice::Fetch { asset, cache } => match FETCHER.get() {
            Some(fetcher) => fetcher
                .fetch(&asset, &cache)
                .map_err(|e| format!("fetching the CUDA module {} failed: {e}", asset.name)),
            None => Err(format!(
                "`{MANIFEST_FILE}` names the CUDA module {}, but this program does not fetch; \
                 `datalib-step pull-cuda` does",
                asset.name
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_manifest::AssetKind;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn asset(kind: AssetKind, sha256: &str) -> RuntimeAsset {
        RuntimeAsset {
            kind,
            name: format!("{}.tar.gz", kind.as_str()),
            sha256: sha256.to_string(),
            bytes: 1,
            url: "https://example.com/a.tar.gz".to_string(),
        }
    }

    fn with_cuda() -> Manifest {
        Manifest::new(asset(AssetKind::Cpu, SHA_A)).with_cuda(asset(AssetKind::Cuda, SHA_B))
    }

    fn seen() -> Seen {
        Seen {
            beside: Some((PathBuf::from("/opt/d/lib/libggml-cuda.so"), false)),
            driver: true,
            manifest: Ok(with_cuda()),
            cache: Some((PathBuf::from("/home/u/.cache/datalib/cuda"), false)),
        }
    }

    #[test]
    fn a_module_beside_the_binary_is_loaded_whatever_else_there_is() {
        let s = Seen {
            beside: Some((PathBuf::from("/opt/d/lib/libggml-cuda.so"), true)),
            driver: false,
            manifest: Err(FetchOutcome::NoManifest),
            cache: None,
        };
        assert_eq!(choose(s), Choice::Load("/opt/d/lib/libggml-cuda.so".into()));
    }

    #[test]
    fn with_a_driver_the_manifests_module_is_fetched_once_then_loaded_from_the_cache() {
        let Choice::Fetch { asset, cache } = choose(seen()) else {
            panic!("{:?}", choose(seen()));
        };
        assert_eq!(asset.kind, AssetKind::Cuda);
        assert_eq!(cache, Path::new("/home/u/.cache/datalib/cuda"));
        let fetched = Seen {
            cache: Some((cache, true)),
            ..seen()
        };
        assert_eq!(
            choose(fetched),
            Choice::Load("/home/u/.cache/datalib/cuda/bbbbbbbbbbbb/lib/libggml-cuda.so".into())
        );
    }

    /// The common case on a laptop or a CPU server: nothing is fetched.
    #[test]
    fn without_a_driver_nothing_is_fetched() {
        let s = Seen {
            driver: false,
            ..seen()
        };
        let Choice::None(why) = choose(s) else {
            panic!()
        };
        assert!(why.contains("no NVIDIA driver (libcuda.so.1)"), "{why}");
        assert!(why.contains("/opt/d/lib/libggml-cuda.so"), "{why}");
    }

    #[test]
    fn a_manifest_without_a_cuda_line_or_none_at_all_says_so() {
        let cpu_only = Seen {
            manifest: Ok(Manifest::new(asset(AssetKind::Cpu, SHA_A))),
            ..seen()
        };
        let Choice::None(why) = choose(cpu_only) else {
            panic!()
        };
        assert!(why.contains("names none for this build"), "{why}");
        let checkout = Seen {
            manifest: Err(FetchOutcome::NoManifest),
            ..seen()
        };
        let Choice::None(why) = choose(checkout) else {
            panic!()
        };
        assert!(why.contains("no `runtime.manifest`"), "{why}");
    }
}
