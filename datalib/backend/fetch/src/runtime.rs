//! The Node runtime a release tarball names in `runtime.manifest`,
//! fetched into the user's cache on first use. `docs/dev/runtime_fetch.md`
//! is the map; `datalib_runtime::node_runtime` decides *when* this runs
//! (only when no `runtime/` sits beside the binaries) and where the
//! result is looked for.
//!
//! Layout under the cache directory:
//!
//! ```text
//! <cache>/CACHEDIR.TAG
//! <cache>/.lock                  one fetch at a time, across processes
//! <cache>/<sha12>/node/bin/node  the CPU asset, named by its sha256
//! <cache>/<sha12>/.cuda-<sha12>  marker: the CUDA asset is overlaid
//! <cache>/<sha12>.part-<pid>     an unpack in progress; renamed whole
//! ```
//!
//! A directory that exists was fetched, verified and unpacked to the
//! end — the rename into place is the last step — so presence is the
//! whole check and nothing is re-hashed on later starts.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_obs::status_line;
use datalib_runtime::layout::mark_derived_cache;
use datalib_runtime::node_runtime::{
    enable_fetch, fetched_runtime_dir, Manifest, RuntimeAsset, RuntimeFetcher,
};

const LOCK_FILE: &str = ".lock";
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
/// bundled is found. Called once, early, by `datalib-step`,
/// `datalib-applet` and `datalib-http`.
pub fn enable_runtime_fetch() {
    enable_fetch(Box::new(ReleaseRuntimeFetcher));
}

/// The manifest's runtime, in place under `cache_dir`: present already,
/// or fetched, verified and unpacked now. Returns the tree's root.
pub fn ensure_runtime(manifest: &Manifest, cache_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    mark_derived_cache(cache_dir);
    let _lock = Lock::take(&cache_dir.join(LOCK_FILE))?;

    let cpu = manifest.cpu();
    let root = fetched_runtime_dir(cache_dir, cpu);
    if !root.is_dir() {
        refuse_musl_host()?;
        status_line!(
            "{TAG} fetching the search runtime ({}, {} MB) — once per release, into {}",
            cpu.name,
            cpu.megabytes(),
            cache_dir.display()
        );
        let part = cache_dir.join(format!("{}.part-{}", cpu.dir_name(), std::process::id()));
        fetch_and_unpack(cache_dir, cpu, &part)?;
        fs::rename(&part, &root)
            .with_context(|| format!("rename {} -> {}", part.display(), root.display()))?;
    }
    if let Some(cuda) = manifest.cuda() {
        let marker = root.join(format!(".cuda-{}", cuda.dir_name()));
        if !marker.exists() && cuda_wanted() {
            status_line!(
                "{TAG} an NVIDIA driver is present: fetching the CUDA runtime ({}, {} MB)",
                cuda.name,
                cuda.megabytes()
            );
            fetch_and_unpack(cache_dir, cuda, &root)?;
            fs::write(&marker, format!("{}\n", cuda.sha256))
                .with_context(|| format!("write {}", marker.display()))?;
        }
    }
    prune(cache_dir, &root)?;
    Ok(root)
}

/// Download `asset` beside the cache and unpack it into `into`, which
/// is created if absent. The archive is removed afterwards either way.
fn fetch_and_unpack(cache_dir: &Path, asset: &RuntimeAsset, into: &Path) -> Result<()> {
    let archive = cache_dir.join(&asset.name);
    let _ = fs::remove_dir_all(into);
    let result = crate::download_verified(TAG, &asset.url, &asset.sha256, &archive)
        .and_then(|()| {
            fs::create_dir_all(into).with_context(|| format!("create {}", into.display()))?;
            crate::unpack_tar_gz(&archive, into)
        })
        .with_context(|| format!("fetch {}", asset.name));
    let _ = fs::remove_file(&archive);
    if result.is_err() {
        let _ = fs::remove_dir_all(into);
    }
    result
}

/// Everything in the cache that is not the live tree, the lock or the
/// tag: earlier releases' trees, and unpack directories a crash left.
fn prune(cache_dir: &Path, keep: &Path) -> Result<()> {
    for entry in fs::read_dir(cache_dir).with_context(|| format!("list {}", cache_dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name();
        if name == LOCK_FILE || name == "CACHEDIR.TAG" {
            continue;
        }
        status_line!("{TAG} removing {}", path.display());
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
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

/// The CUDA overlay is wanted when qmd is told to use CUDA, or when the
/// driver's user-space library is on the loader path — the same test
/// qmd's `auto` mode ends up making, only before the 500 MB download.
fn cuda_wanted() -> bool {
    if std::env::var("QMD_LLAMA_GPU").as_deref() == Ok("cuda") {
        return true;
    }
    if !cfg!(target_os = "linux") {
        return false;
    }
    std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("libcuda.so.1"))
        .unwrap_or(false)
}

/// A blocking `flock(2)` on the cache: the process that gets there
/// first fetches, the others wait and find the tree in place. Released
/// on drop, or by the kernel if the holder dies.
struct Lock(fs::File);

impl Lock {
    fn take(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        loop {
            // SAFETY: a valid, open descriptor; flock has no other
            // preconditions.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Self(file));
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).with_context(|| format!("lock {}", path.display()));
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: as above; the descriptor is ours until `self.0` drops.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stale trees and interrupted unpacks go; the live tree, the lock
    /// and the backup tag stay.
    #[test]
    fn prune_keeps_only_the_live_tree() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("aaaaaaaaaaaa");
        for d in ["aaaaaaaaaaaa", "bbbbbbbbbbbb", "aaaaaaaaaaaa.part-123"] {
            fs::create_dir_all(dir.path().join(d).join("node")).unwrap();
        }
        fs::write(dir.path().join(LOCK_FILE), "").unwrap();
        fs::write(dir.path().join("CACHEDIR.TAG"), "sig").unwrap();
        fs::write(dir.path().join("runtime-x.tar.gz.partial"), "half").unwrap();
        prune(dir.path(), &live).unwrap();
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [".lock", "CACHEDIR.TAG", "aaaaaaaaaaaa"]);
    }
}
