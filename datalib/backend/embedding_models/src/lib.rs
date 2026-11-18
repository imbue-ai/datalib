//! Provision the pinned GGUF models
//! (`datalib_runtime::models::PINNED_MODELS`): verify what is on disk by
//! sha256, fetch what is missing or wrong from the pinned revision, and
//! leave the files under the names node-llama-cpp gave them, where the
//! copies qmd downloaded already are.
//!
//! A root where something else staged the models sets
//! `DATALIB_MODELS_NO_FETCH=1` (`Fetch::from_env`), and an absent
//! model is then reported rather than downloaded. The e2e fixture is
//! the case: it stages the embedding model alone, because that is all
//! the suite loads, and the other two are 1.8 GB.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use datalib_obs::status_line;
use datalib_runtime::models::PinnedModel;

pub use datalib_fetch::sha256_file;
pub use datalib_runtime::models::PINNED_MODELS;

const TAG: &str = "[models]";

/// Where the models are for a search of, or an embed into, the data root
/// at `root`: its own `unified_grid_and_search_index/models` when it has one (the
/// test fixtures stage the model there), else the cache every root on
/// this machine shares ([`shared_models_dir`]).
pub fn models_dir_for(root: &Path) -> PathBuf {
    let own = datalib_runtime::layout::unified_grid_and_search_index_dir(root).join("models");
    if own.is_dir() {
        own
    } else {
        shared_models_dir()
    }
}

/// The cache every data root on this machine shares:
/// `$XDG_CACHE_HOME/qmd/models`, else `~/.cache/qmd/models`. qmd chose the
/// place, and the models already downloaded there are kept.
pub fn shared_models_dir() -> PathBuf {
    models_dir_under(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

fn models_dir_under(
    xdg_cache_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(xdg) = xdg_cache_home {
        return PathBuf::from(xdg).join("qmd").join("models");
    }
    PathBuf::from(home.unwrap_or_else(|| ".".into()))
        .join(".cache")
        .join("qmd")
        .join("models")
}

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
    /// Absent, or present and hashing wrong, with fetching off
    /// (`Fetch::Never`). Nothing was written or removed.
    Missing,
}

/// Whether `ensure_models` may go to the network for a model that is
/// absent or hashes wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Allowed,
    Never,
}

pub const NO_FETCH_ENV: &str = "DATALIB_MODELS_NO_FETCH";
/// [`NO_FETCH_ENV`]'s name while qmd loaded the models, still honoured
/// for a script or a test runner that sets it.
pub const RETIRED_NO_FETCH_ENV: &str = "DATALIB_QMD_MODELS_NO_FETCH";

impl Fetch {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(NO_FETCH_ENV).ok().as_deref(),
            std::env::var(RETIRED_NO_FETCH_ENV).ok().as_deref(),
        )
    }

    /// `Never` when the switch is set to anything but empty or `0`; its
    /// current name wins over its retired one when both are set.
    fn from_values(current: Option<&str>, retired: Option<&str>) -> Self {
        match current.or(retired) {
            Some(v) if !v.is_empty() && v != "0" => Fetch::Never,
            _ => Fetch::Allowed,
        }
    }
}

/// Every model in `models` is in `models_dir` under its cache name with
/// the pinned sha256 when this returns `Ok` — unless `fetch` is `Never`,
/// in which case the ones that are not come back as `Outcome::Missing`
/// and the caller decides. A file that hashes wrong is said so on
/// stderr and re-fetched; a fetch that hashes wrong is an error, never
/// a file left behind.
pub fn ensure_models(
    models_dir: &Path,
    models: &[PinnedModel],
    fetch: Fetch,
) -> Result<Vec<Outcome>> {
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create models dir {}", models_dir.display()))?;
    let mut out = Vec::with_capacity(models.len());
    for model in models {
        out.push(ensure_one(models_dir, model, fetch)?);
    }
    Ok(out)
}

/// The cache names of the models `outcomes` reported `Missing`, for a
/// log line.
pub fn missing<'a>(models: &'a [PinnedModel], outcomes: &[Outcome]) -> Vec<&'a str> {
    models
        .iter()
        .zip(outcomes)
        .filter(|(_, o)| **o == Outcome::Missing)
        .map(|(m, _)| m.file)
        .collect()
}

fn ensure_one(models_dir: &Path, model: &PinnedModel, fetch: Fetch) -> Result<Outcome> {
    let path = models_dir.join(model.cache_name());
    let present = fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false);
    if present && verified(&path, model.sha256)? {
        return Ok(Outcome::Verified);
    }
    if fetch == Fetch::Never {
        status_line!(
            "[models] {} is {} and fetching is off ({NO_FETCH_ENV}); leaving it",
            path.display(),
            if present {
                "not the pinned bytes"
            } else {
                "absent"
            }
        );
        return Ok(Outcome::Missing);
    }
    if present {
        status_line!(
            "{TAG} {} does not match the pinned sha256 for {}@{} — replacing it",
            path.display(),
            model.repo,
            model.revision
        );
        // Another process that found it wrong too may have removed it first.
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("remove {}", path.display()));
            }
            _ => {}
        }
        let _ = fs::remove_file(stamp_path(&path));
    }
    status_line!(
        "{TAG} fetching {} ({} @ {})",
        model.cache_name(),
        model.repo,
        &model.revision[..12.min(model.revision.len())]
    );
    datalib_fetch::download_verified(TAG, &model.url(), model.sha256, &path)?;
    write_stamp(&path, model.sha256)?;
    Ok(if present {
        Outcome::Replaced
    } else {
        Outcome::Downloaded
    })
}

/// True when `path` hashes to `want`. Hashing a 300 MB model on every sync is
/// real time, so a passing hash is stamped beside the file with the
/// size and mtime it had; the same size and mtime next time means the
/// same bytes, short of someone editing the file and resetting its
/// mtime, which is not the threat this guards.
///
/// The mtime is recorded to the nanosecond, not the second: a stamp
/// that is more precise than it needs to be only ever costs an extra
/// hash, while one that is less precise lets changed bytes through, so
/// the two errors are not worth trading evenly. Nanoseconds survive a
/// Docker layer, which is where the saving matters most — the image
/// stamps the models at build time and a container must not re-hash
/// them.
fn verified(path: &Path, want: &str) -> Result<bool> {
    if stamp_matches(path, want) {
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

/// The file's size and mtime, the pair a stamp is keyed to.
///
/// `None` when either cannot be read — an mtime the platform will not
/// give, or one before 1970. Nothing is then trusted and nothing is
/// stamped, so the bytes are hashed. Reporting such a file as mtime
/// zero, which this used to do, meant a second unreadable mtime at the
/// same size matched the stamp and the bytes went unchecked.
fn file_identity(path: &Path) -> Option<(u64, u128)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), mtime.as_nanos()))
}

fn stamp_matches(path: &Path, want: &str) -> bool {
    let Some((len, mtime)) = file_identity(path) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(stamp_path(path)) else {
        return false;
    };
    text.trim() == stamp_text(want, len, mtime)
}

fn stamp_text(sha: &str, len: u64, mtime: u128) -> String {
    format!("{sha} {len} {mtime}")
}

fn write_stamp(path: &Path, sha: &str) -> Result<()> {
    // A stamp nothing is keyed to would be read back and trusted, so a
    // file whose identity will not read simply goes unstamped: the hash
    // that just passed is the only thing vouching for it.
    let Some((len, mtime)) = file_identity(path) else {
        return Ok(());
    };
    let stamp = stamp_path(path);
    fs::write(&stamp, format!("{}\n", stamp_text(sha, len, mtime)))
        .with_context(|| format!("write {}", stamp.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test runner or script that still sets the switch's old name keeps
    /// the models from being fetched; the new name, set either way, wins.
    #[test]
    fn the_no_fetch_switch_reads_its_old_name_too() {
        let fetch = Fetch::from_values;
        assert_eq!(fetch(None, None), Fetch::Allowed);
        assert_eq!(fetch(Some("1"), None), Fetch::Never);
        assert_eq!(fetch(None, Some("1")), Fetch::Never);
        assert_eq!(fetch(Some("0"), Some("1")), Fetch::Allowed);
        assert_eq!(fetch(Some("1"), Some("0")), Fetch::Never);
        assert_eq!(fetch(Some(""), None), Fetch::Allowed);
    }

    /// The shared cache is where qmd kept its models, so the ones it
    /// downloaded are reused: `$XDG_CACHE_HOME` first, then `$HOME`.
    #[test]
    fn the_shared_cache_is_where_qmd_kept_its_models() {
        assert_eq!(
            models_dir_under(Some("/x/cache".into()), Some("/home/u".into())),
            PathBuf::from("/x/cache/qmd/models")
        );
        assert_eq!(
            models_dir_under(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.cache/qmd/models")
        );
        assert_eq!(
            models_dir_under(None, None),
            PathBuf::from("./.cache/qmd/models")
        );
    }

    /// A root that carries its own models is searched with them; one
    /// that does not uses the shared cache.
    #[test]
    fn a_root_with_its_own_models_uses_them() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(models_dir_for(root.path()), shared_models_dir());
        let own = root.path().join("unified_grid_and_search_index/models");
        std::fs::create_dir_all(&own).unwrap();
        assert_eq!(models_dir_for(root.path()), own);
    }

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
        assert!(stamp_matches(&path, HELLO_SHA));

        fs::write(&path, b"hello, world").unwrap();
        assert!(!stamp_matches(&path, HELLO_SHA));
        assert!(!verified(&path, HELLO_SHA).unwrap());
    }

    /// What spares an applet start from re-reading the model: a stamp matching
    /// the file's size and mtime is trusted without hashing, so other
    /// bytes under that size and mtime pass. Moving the mtime alone is
    /// enough to send the file back to the hash.
    #[test]
    fn a_matching_stamp_skips_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, b"hello").unwrap();
        assert!(verified(&path, HELLO_SHA).unwrap());
        let stamped_mtime = fs::metadata(&path).unwrap().modified().unwrap();

        let set_mtime = |mtime| {
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(mtime).unwrap();
        };
        fs::write(&path, b"jello").unwrap();
        set_mtime(stamped_mtime);
        assert!(verified(&path, HELLO_SHA).unwrap());

        set_mtime(stamped_mtime + std::time::Duration::from_secs(2));
        assert!(!verified(&path, HELLO_SHA).unwrap());
    }

    /// Sub-second granularity is the point: the stamp recorded whole
    /// seconds until now, so bytes replaced inside the same second as
    /// the stamped mtime, at the same size, were trusted without being
    /// hashed. The mtime here is set to a whole second so that adding a
    /// millisecond cannot cross into the next one and pass for the wrong
    /// reason.
    #[test]
    fn a_change_inside_the_stamped_second_goes_back_to_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, b"hello").unwrap();
        let set_mtime = |t| {
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        let whole_second = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        set_mtime(whole_second);
        assert!(verified(&path, HELLO_SHA).unwrap(), "hashes and stamps");

        // Same size, same second, different bytes.
        fs::write(&path, b"jello").unwrap();
        set_mtime(whole_second + std::time::Duration::from_millis(1));
        assert!(!verified(&path, HELLO_SHA).unwrap());
    }

    /// An mtime that will not read is not an identity, so nothing is
    /// stamped and every call hashes. Recording it as zero, as this used
    /// to, let a second such file at the same size match the stamp.
    #[test]
    fn a_file_with_no_readable_mtime_is_never_trusted_from_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, b"hello").unwrap();
        let before_the_epoch = UNIX_EPOCH - std::time::Duration::from_secs(1);
        let set_mtime = |t| {
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        set_mtime(before_the_epoch);
        // Asserted rather than skipped: reporting such a file as mtime
        // zero also reads back as an identity, so a `return` here would
        // make this test pass against the very behaviour it guards.
        assert!(
            file_identity(&path).is_none(),
            "an mtime before 1970 is not an identity"
        );
        assert!(verified(&path, HELLO_SHA).unwrap(), "the hash still passes");
        assert!(
            !stamp_path(&path).exists(),
            "a file with no identity must not be stamped"
        );

        // The bytes change, the unreadable mtime does not: it must hash.
        fs::write(&path, b"jello").unwrap();
        set_mtime(before_the_epoch);
        assert!(!verified(&path, HELLO_SHA).unwrap());
    }

    /// A stamp for one digest must not vouch for another.
    #[test]
    fn stamp_is_bound_to_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, b"hello").unwrap();
        assert!(verified(&path, HELLO_SHA).unwrap());
        assert!(!stamp_matches(&path, &"0".repeat(64)));
    }

    /// With fetching off, an absent model is reported, not downloaded,
    /// and a present one is still verified — the e2e fixture relies on
    /// both halves.
    #[test]
    fn no_fetch_reports_absent_and_verifies_present() {
        let dir = tempfile::tempdir().unwrap();
        let models = [
            PinnedModel {
                repo: "o/present",
                revision: "r",
                file: "present.gguf",
                sha256: HELLO_SHA,
            },
            PinnedModel {
                repo: "o/absent",
                revision: "r",
                file: "absent.gguf",
                sha256: HELLO_SHA,
            },
        ];
        fs::write(dir.path().join(models[0].cache_name()), b"hello").unwrap();
        let outcomes = ensure_models(dir.path(), &models, Fetch::Never).unwrap();
        assert_eq!(outcomes, [Outcome::Verified, Outcome::Missing]);
        assert_eq!(missing(&models, &outcomes), ["absent.gguf"]);
        assert!(!dir.path().join(models[1].cache_name()).exists());
    }
}
