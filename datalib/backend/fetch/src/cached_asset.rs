//! A release asset fetched into a cache directory on first use, unpacked
//! into a directory named by its sha256, the one tree of its kind the
//! directory keeps. The Node runtime ([`crate::runtime`]) and the CUDA
//! module ([`crate::cuda`]) each have a directory of their own.
//!
//! ```text
//! <cache>/CACHEDIR.TAG
//! <cache>/.lock                  one fetch at a time, across processes
//! <cache>/<sha12>/               the asset, named by its sha256
//! <cache>/<sha12>.part-<pid>     an unpack in progress; renamed whole
//! ```
//!
//! A directory that exists was fetched, verified and unpacked to the
//! end — the rename into place is the last step — so presence is the
//! whole check and nothing is re-hashed on later starts.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_obs::status_line;
use datalib_runtime::layout::mark_cache;
use datalib_runtime::node_runtime::RuntimeAsset;

const LOCK_FILE: &str = ".lock";

/// `asset` unpacked at `<cache_dir>/<sha12>/`: there already, or fetched
/// now, `before_fetch` first. Everything else in `cache_dir` goes.
pub(crate) fn ensure(
    cache_dir: &Path,
    asset: &RuntimeAsset,
    tag: &str,
    what: &str,
    before_fetch: fn() -> Result<()>,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    mark_cache(cache_dir);
    let _lock = Lock::take(&cache_dir.join(LOCK_FILE))?;

    let root = cache_dir.join(asset.dir_name());
    if !root.is_dir() {
        before_fetch()?;
        status_line!(
            "{tag} fetching {what} ({}, {} MB) — once per release, into {}",
            asset.name,
            asset.megabytes(),
            cache_dir.display()
        );
        let part = cache_dir.join(format!("{}.part-{}", asset.dir_name(), std::process::id()));
        fetch_and_unpack(cache_dir, asset, tag, &part)?;
        fs::rename(&part, &root)
            .with_context(|| format!("rename {} -> {}", part.display(), root.display()))?;
    }
    prune(cache_dir, &root, tag)?;
    Ok(root)
}

/// Download `asset` beside the cache and unpack it into `into`, which
/// is created if absent. The archive is removed afterwards either way.
fn fetch_and_unpack(cache_dir: &Path, asset: &RuntimeAsset, tag: &str, into: &Path) -> Result<()> {
    let archive = cache_dir.join(&asset.name);
    let _ = fs::remove_dir_all(into);
    let result = crate::download_verified(tag, &asset.url, &asset.sha256, &archive)
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
fn prune(cache_dir: &Path, keep: &Path, tag: &str) -> Result<()> {
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
        status_line!("{tag} removing {}", path.display());
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
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
        prune(dir.path(), &live, "[test]").unwrap();
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [".lock", "CACHEDIR.TAG", "aaaaaaaaaaaa"]);
    }
}
