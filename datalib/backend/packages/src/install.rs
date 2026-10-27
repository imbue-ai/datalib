//! Looking a package up and unpacking it under the data root. A lookup
//! fetches only the manifest, for the person to review; `ensure` fetches
//! the build the pinned manifest names for this platform, checks it, and
//! unpacks it where the config's steps look for it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::manifest::{is_sha256, Artifact, Manifest};
use crate::platform::this_target;
use crate::source;

/// The largest manifest a lookup reads.
const MANIFEST_LIMIT: u64 = 1 << 20;

/// The copy of the manifest kept beside the unpacked build, and the stamp
/// that says the unpack finished and for which platform. Dot-named so they
/// cannot collide with a package's own files.
const MANIFEST_COPY: &str = ".datalib-package.toml";
const STAMP: &str = ".datalib-installed";

/// A manifest fetched for review: what it says, and the pin that adding
/// it would write.
#[derive(Debug, Clone, Serialize)]
pub struct Lookup {
    /// The manifest's URL after redirects — a release's own URL, never
    /// `latest` — which is what the config pins.
    pub url: String,
    pub sha256: String,
    pub manifest: Manifest,
    /// This platform, and the build it would get (`None`: the package
    /// has none for it).
    pub target: &'static str,
    pub artifact: Option<(String, Artifact)>,
}

pub fn lookup(source_text: &str) -> Result<Lookup> {
    let url = source::manifest_url(source_text)?;
    let fetched = read_url(&url)?;
    let text = std::str::from_utf8(&fetched.bytes).context("the manifest is not UTF-8")?;
    let manifest = Manifest::parse(text)?;
    let target = this_target();
    let artifact = manifest
        .artifact_for(target)
        .map(|(k, a)| (k.to_string(), a.clone()));
    Ok(Lookup {
        url: fetched.final_url,
        sha256: fetched.sha256,
        manifest,
        target,
        artifact,
    })
}

/// The package pinned at `sha256`, if it is unpacked here for this platform.
pub fn installed(data_root: &Path, sha256: &str) -> Option<Manifest> {
    let dir = package_dir(data_root, sha256);
    let stamp = fs::read_to_string(dir.join(STAMP)).ok()?;
    if stamp.trim() != this_target() {
        return None;
    }
    let text = fs::read_to_string(dir.join(MANIFEST_COPY)).ok()?;
    Manifest::parse(&text).ok()
}

/// Make the package pinned at (`url`, `sha256`) present under the data
/// root, fetching it if it is not. A manifest that no longer hashes to the
/// pin is refused: what was reviewed is what runs.
pub fn ensure(data_root: &Path, url: &str, sha256: &str) -> Result<Manifest> {
    if !is_sha256(sha256) {
        bail!("sha256 {sha256:?} is not a sha256");
    }
    if let Some(m) = installed(data_root, sha256) {
        return Ok(m);
    }
    let fetched = read_url(url)?;
    if fetched.sha256 != sha256 {
        bail!(
            "the manifest at {url} is not the one this library added (sha256 {}, pinned {sha256}); \
             look the package up again to review what changed",
            fetched.sha256
        );
    }
    let text = std::str::from_utf8(&fetched.bytes).context("the manifest is not UTF-8")?;
    let manifest = Manifest::parse(text)?;
    let target = this_target();
    let Some((_, artifact)) = manifest.artifact_for(target) else {
        bail!(
            "{} {} has no build for {target}; it has {}",
            manifest.name,
            manifest.version,
            manifest
                .artifacts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let artifact_url = source::join(&fetched.final_url, &artifact.url)?;

    let dir = package_dir(data_root, sha256);
    let parent = dir.parent().expect("a package dir has a parent");
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(parent)
        .context("make a staging directory")?;
    let archive = staging.path().join("build.tar.gz");
    fetch_pinned(&artifact_url, &artifact.sha256, &archive)?;
    let unpacked = staging.path().join("unpacked");
    fs::create_dir(&unpacked)?;
    datalib_fetch::unpack_tar_gz(&archive, &unpacked)?;
    fs::write(unpacked.join(MANIFEST_COPY), &fetched.bytes)?;
    fs::write(unpacked.join(STAMP), target)?;
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    fs::rename(&unpacked, &dir).with_context(|| format!("move into {}", dir.display()))?;
    Ok(manifest)
}

fn package_dir(data_root: &Path, sha256: &str) -> PathBuf {
    data_root.join(datalib_runtime::layout::package_dir_rel(sha256))
}

struct Fetched {
    bytes: Vec<u8>,
    sha256: String,
    final_url: String,
}

fn read_url(url: &str) -> Result<Fetched> {
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = fs::read(path).with_context(|| format!("read {path}"))?;
        if bytes.len() as u64 > MANIFEST_LIMIT {
            bail!("{path}: larger than {MANIFEST_LIMIT} bytes");
        }
        return Ok(Fetched {
            sha256: datalib_fetch::sha256_bytes(&bytes),
            bytes,
            final_url: url.to_string(),
        });
    }
    let u = datalib_fetch::get_unpinned(url, MANIFEST_LIMIT)?;
    Ok(Fetched {
        bytes: u.bytes,
        sha256: u.sha256,
        final_url: u.final_url,
    })
}

fn fetch_pinned(url: &str, sha256: &str, dest: &Path) -> Result<()> {
    match url.strip_prefix("file://") {
        Some(path) => {
            fs::copy(path, dest).with_context(|| format!("copy {path}"))?;
            let got = datalib_fetch::sha256_file(dest)?;
            if got != sha256 {
                bail!("{url}: sha256 {got}, the manifest says {sha256}");
            }
            Ok(())
        }
        None => datalib_fetch::download_verified("[packages]", url, sha256, dest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package with one script, built and published as files, so the
    /// whole lookup → ensure path runs without a network.
    fn publish(dir: &Path, script: &str) -> String {
        let tar_path = dir.join("weather.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&tar_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "bin/fetch-weather", script.as_bytes())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let sha = datalib_fetch::sha256_file(&tar_path).unwrap();
        let manifest = format!(
            "schema = 1\nname = \"Weather\"\nversion = \"1.2.0\"\n\
             [artifacts]\nany = {{ url = \"weather.tar.gz\", sha256 = \"{sha}\" }}\n"
        );
        let m = dir.join("datalib-package.toml");
        fs::write(&m, manifest).unwrap();
        format!("file://{}", m.display())
    }

    #[test]
    fn lookup_then_ensure_unpacks_under_the_pin() {
        let published = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let url = publish(published.path(), "#!/bin/sh\necho hi\n");

        let found = lookup(&url).unwrap();
        assert_eq!(found.manifest.name, "Weather");
        assert!(installed(root.path(), &found.sha256).is_none());

        let m = ensure(root.path(), &found.url, &found.sha256).unwrap();
        assert_eq!(m.version, "1.2.0");
        let program = root
            .path()
            .join(datalib_runtime::layout::package_dir_rel(&found.sha256))
            .join("bin/fetch-weather");
        assert!(program.is_file(), "{}", program.display());
        assert!(installed(root.path(), &found.sha256).is_some());
    }

    /// The manifest changing after it was reviewed must not change what
    /// runs: ensure refuses rather than unpacking the new one.
    #[test]
    fn a_manifest_that_moved_since_review_is_refused() {
        let published = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let url = publish(published.path(), "#!/bin/sh\necho hi\n");
        let found = lookup(&url).unwrap();
        publish(published.path(), "#!/bin/sh\necho changed\n");
        let e = ensure(root.path(), &found.url, &found.sha256)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not the one this library added"), "{e}");
        assert!(installed(root.path(), &found.sha256).is_none());
    }
}
