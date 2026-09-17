//! Provision qmd's GGUF models from the pinned table
//! (`datalib_runtime::qmd::PINNED_MODELS`): verify what is on disk by
//! sha256, fetch what is missing or wrong from the pinned revision, and
//! leave the files under the names node-llama-cpp looks for — so qmd
//! finds every model already in place and never talks to HuggingFace.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use datalib_obs::status_line;
use datalib_runtime::qmd::PinnedModel;
use sha2::{Digest, Sha256};

pub use datalib_runtime::qmd::PINNED_MODELS;

const CHUNK: usize = 1 << 20;
const ATTEMPTS: u32 = 6;

/// What `ensure_models` did for each model, for the caller's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Present and its sha256 matched (or matched when last hashed and
    /// the file has not changed since).
    Verified,
    /// Fetched from the pinned revision and verified.
    Downloaded,
    /// Present but its sha256 did not match the pin; replaced.
    Replaced,
}

/// Where a data root's qmd actually reads its models: the target of the
/// `<qmd_state_dir>/models` link when one exists (the indexer made it
/// on an earlier run, or the test fixture pointed it at bazel outputs),
/// else `default` — the directory the indexer will link on this run.
pub fn effective_models_dir(qmd_state_dir: &Path, default: &Path) -> PathBuf {
    let link = qmd_state_dir.join("models");
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(&link).unwrap_or(link),
        _ => default.to_path_buf(),
    }
}

/// Every model in `models` is in `models_dir` under its cache name with
/// the pinned sha256 when this returns `Ok`. A file that hashes wrong is
/// said so on stderr and re-fetched; a fetch that hashes wrong is an
/// error, never a file left behind.
pub fn ensure_models(models_dir: &Path, models: &[PinnedModel]) -> Result<Vec<Outcome>> {
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create models dir {}", models_dir.display()))?;
    let mut out = Vec::with_capacity(models.len());
    for model in models {
        out.push(ensure_one(models_dir, model)?);
    }
    Ok(out)
}

fn ensure_one(models_dir: &Path, model: &PinnedModel) -> Result<Outcome> {
    let path = models_dir.join(model.cache_name());
    let present = fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false);
    if present {
        if verified(&path, model.sha256)? {
            return Ok(Outcome::Verified);
        }
        status_line!(
            "[qmd-models] {} does not match the pinned sha256 for {}@{} — replacing it",
            path.display(),
            model.repo,
            model.revision
        );
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        let _ = fs::remove_file(stamp_path(&path));
    }
    status_line!(
        "[qmd-models] fetching {} ({} @ {})",
        model.cache_name(),
        model.repo,
        &model.revision[..12.min(model.revision.len())]
    );
    download_verified(&model.url(), model.sha256, &path)?;
    write_stamp(&path, model.sha256)?;
    Ok(if present {
        Outcome::Replaced
    } else {
        Outcome::Downloaded
    })
}

/// True when `path` hashes to `want`. Hashing 2 GB on every sync is
/// real time, so a passing hash is stamped beside the file with the
/// size and mtime it had; the same size and mtime next time means the
/// same bytes, short of someone editing the file and resetting its
/// mtime, which is not the threat this guards.
fn verified(path: &Path, want: &str) -> Result<bool> {
    if stamp_matches(path, want)? {
        return Ok(true);
    }
    let got = sha256_file(path)?;
    if got != want {
        return Ok(false);
    }
    write_stamp(path, want)?;
    Ok(true)
}

fn stamp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256-ok");
    path.with_file_name(name)
}

fn file_identity(path: &Path) -> Result<(u64, u64)> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

fn stamp_matches(path: &Path, want: &str) -> Result<bool> {
    let Ok(text) = fs::read_to_string(stamp_path(path)) else {
        return Ok(false);
    };
    let (len, mtime) = file_identity(path)?;
    Ok(text.trim() == stamp_text(want, len, mtime))
}

fn stamp_text(sha: &str, len: u64, mtime: u64) -> String {
    format!("{sha} {len} {mtime}")
}

fn write_stamp(path: &Path, sha: &str) -> Result<()> {
    let (len, mtime) = file_identity(path)?;
    let stamp = stamp_path(path);
    fs::write(&stamp, format!("{}\n", stamp_text(sha, len, mtime)))
        .with_context(|| format!("write {}", stamp.display()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// GET `url` to `<dest>.partial`, hashing as it streams; rename into
/// place only on a matching digest. HuggingFace has answered a cold
/// fetch with 429 before, so a retryable status backs off and tries
/// again rather than failing the sync on the first refusal.
fn download_verified(url: &str, want: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // The whole-request timeout would cap a multi-GB download; a
        // stalled body read is caught by the connection's own timeouts
        // and retried below.
        .timeout(None)
        .user_agent(concat!("datalib/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        match fetch_once(&client, url, want, dest) {
            Ok(()) => return Ok(()),
            Err(Retry(reason)) if attempt < ATTEMPTS => {
                status_line!(
                    "[qmd-models] {url}: {reason}; retrying in {}s ({attempt}/{ATTEMPTS})",
                    delay.as_secs()
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(Retry(reason)) => bail!("{url}: {reason} (gave up after {ATTEMPTS} attempts)"),
            Err(Fatal(e)) => return Err(e),
        }
    }
    unreachable!("the loop returns or bails on its last attempt")
}

enum FetchError {
    Retry(String),
    Fatal(anyhow::Error),
}
use FetchError::{Fatal, Retry};

impl From<anyhow::Error> for FetchError {
    fn from(e: anyhow::Error) -> Self {
        Fatal(e)
    }
}

fn fetch_once(
    client: &reqwest::blocking::Client,
    url: &str,
    want: &str,
    dest: &Path,
) -> std::result::Result<(), FetchError> {
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| Retry(format!("request failed: {e}")))?;
    let status = resp.status();
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(Retry(format!("HTTP {status}")));
    }
    if !status.is_success() {
        return Err(Fatal(anyhow::anyhow!("{url}: HTTP {status}")));
    }
    let partial = dest.with_extension("partial");
    let mut file =
        fs::File::create(&partial).with_context(|| format!("create {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match resp.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&partial);
                return Err(Retry(format!("read failed mid-body: {e}")));
            }
        };
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("write {}", partial.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("sync {}", partial.display()))?;
    drop(file);
    let got = hex(&hasher.finalize());
    if got != want {
        let _ = fs::remove_file(&partial);
        return Err(Fatal(anyhow::anyhow!(
            "{url}: sha256 {got}, expected {want} — the pinned revision no longer serves \
             the pinned bytes, or something rewrote the download; nothing was kept"
        )));
    }
    fs::rename(&partial, dest)
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// A file that hashes right is stamped; the stamp is trusted while
    /// size and mtime hold; a rewritten file with a different size is
    /// re-hashed and rejected.
    #[test]
    fn verify_stamps_and_rechecks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hf_x_model.gguf");
        fs::write(&path, b"hello").unwrap();
        assert!(!stamp_path(&path).exists());
        assert!(verified(&path, HELLO_SHA).unwrap());
        assert!(stamp_path(&path).exists());
        assert!(stamp_matches(&path, HELLO_SHA).unwrap());

        fs::write(&path, b"hello, world").unwrap();
        assert!(!stamp_matches(&path, HELLO_SHA).unwrap());
        assert!(!verified(&path, HELLO_SHA).unwrap());
    }

    /// A stamp for one digest must not vouch for another.
    #[test]
    fn stamp_is_bound_to_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, b"hello").unwrap();
        assert!(verified(&path, HELLO_SHA).unwrap());
        assert!(!stamp_matches(&path, &"0".repeat(64)).unwrap());
    }

    #[test]
    fn effective_dir_follows_an_existing_link() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("qmd");
        let target = dir.path().join("elsewhere");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&target).unwrap();
        let default = dir.path().join("default");
        assert_eq!(effective_models_dir(&state, &default), default);
        std::os::unix::fs::symlink(&target, state.join("models")).unwrap();
        assert_eq!(
            effective_models_dir(&state, &default),
            fs::canonicalize(&target).unwrap()
        );
    }

    #[test]
    fn sha256_of_known_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), HELLO_SHA);
    }
}
