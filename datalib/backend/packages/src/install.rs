//! Looking a package up and unpacking it under the data root. A lookup
//! fetches only the manifest, for the person to review; `ensure` fetches
//! the build the pinned manifest names for this platform, checks it, and
//! unpacks it where the config's steps look for it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use url::Url;

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

/// Held in the packages directory while a package is unpacked into it.
const LOCK: &str = ".lock";

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
    let url = source::manifest(&source::manifest_url(source_text)?)?;
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
    let fetched = read_url(&source::manifest(url)?)?;
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
    // Two fetches of one package at once would each remove the other's
    // directory; the second waits here and finds it installed.
    let lock = fs::File::create(parent.join(LOCK)).context("open the packages lock")?;
    lock.lock().context("lock the packages directory")?;
    if let Some(m) = installed(data_root, sha256) {
        return Ok(m);
    }
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(parent)
        .context("make a staging directory")?;
    let archive = staging.path().join("build.tar.gz");
    fetch_pinned(&artifact_url, &artifact.sha256, &archive)?;
    let unpacked = staging.path().join("unpacked");
    fs::create_dir(&unpacked)?;
    datalib_fetch::unpack_tar_gz(&archive, &unpacked)?;
    write_new(&unpacked.join(MANIFEST_COPY), &fetched.bytes)?;
    write_new(&unpacked.join(STAMP), target.as_bytes())?;
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    fs::rename(&unpacked, &dir).with_context(|| format!("move into {}", dir.display()))?;
    Ok(manifest)
}

/// Write `bytes` to a new file at `path`, removing whatever the archive
/// put there first: `fs::write` would follow a symlink of the package's
/// and write through it.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("remove {}", path.display()));
        }
        _ => {}
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("write {}", path.display()))
}

fn package_dir(data_root: &Path, sha256: &str) -> PathBuf {
    data_root.join(datalib_runtime::layout::package_dir_rel(sha256))
}

struct Fetched {
    bytes: Vec<u8>,
    sha256: String,
    final_url: String,
}

fn read_url(url: &Url) -> Result<Fetched> {
    if url.scheme() == "file" {
        let path = local_path(url)?;
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() as u64 > MANIFEST_LIMIT {
            bail!("{}: larger than {MANIFEST_LIMIT} bytes", path.display());
        }
        return Ok(Fetched {
            sha256: datalib_fetch::sha256_bytes(&bytes),
            bytes,
            final_url: url.to_string(),
        });
    }
    let u = datalib_fetch::get_unpinned(url.as_str(), MANIFEST_LIMIT)?;
    Ok(Fetched {
        bytes: u.bytes,
        sha256: u.sha256,
        final_url: u.final_url,
    })
}

fn fetch_pinned(url: &Url, sha256: &str, dest: &Path) -> Result<()> {
    if url.scheme() != "file" {
        return datalib_fetch::download_verified("[packages]", url.as_str(), sha256, dest);
    }
    let path = local_path(url)?;
    fs::copy(&path, dest).with_context(|| format!("copy {}", path.display()))?;
    let got = datalib_fetch::sha256_file(dest)?;
    if got != sha256 {
        bail!("{url}: sha256 {got}, the manifest says {sha256}");
    }
    Ok(())
}

fn local_path(url: &Url) -> Result<PathBuf> {
    url.to_file_path()
        .map_err(|()| anyhow::anyhow!("{url} is not a path on this machine"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package with one script, built and published as files, so the
    /// whole lookup → ensure path runs without a network.
    fn publish(dir: &Path, script: &str) -> String {
        publish_with(dir, script, &[])
    }

    /// A link the archive carries beside the script.
    enum Link<'a> {
        Symbolic(&'a str, &'a str),
        Hard(&'a str, &'a str),
    }

    fn publish_with(dir: &Path, script: &str, links: &[Link]) -> String {
        let tar_path = dir.join("weather.tar.gz");
        write_build(&tar_path, script, links);
        let sha = datalib_fetch::sha256_file(&tar_path).unwrap();
        let manifest = format!(
            "schema = 1\nname = \"Weather\"\nversion = \"1.2.0\"\n\
             [artifacts]\nany = {{ url = \"weather.tar.gz\", sha256 = \"{sha}\" }}\n"
        );
        let m = dir.join("datalib-package.toml");
        fs::write(&m, manifest).unwrap();
        format!("file://{}", m.display())
    }

    fn write_build(tar_path: &Path, script: &str, links: &[Link]) {
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(tar_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "bin/fetch-weather", script.as_bytes())
            .unwrap();
        for link in links {
            let (kind, at, target) = match link {
                Link::Symbolic(at, target) => (tar::EntryType::Symlink, at, target),
                Link::Hard(at, target) => (tar::EntryType::Link, at, target),
            };
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            tar.append_link(&mut header, at, target).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
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

    /// `POST /api/packages/fetch` passes its url straight here: a pin
    /// that is not https or a local file is refused before anything is
    /// fetched.
    #[test]
    fn a_pin_over_http_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let sha = "a".repeat(64);
        for url in [
            "http://127.0.0.1:1/datalib-package.toml",
            "ftp://h/datalib-package.toml",
        ] {
            let e = ensure(root.path(), url, &sha).unwrap_err().to_string();
            assert!(e.contains("https"), "{url}: {e}");
        }
    }

    /// An archive carrying a symlink at the name of the manifest copy or
    /// the stamp, pointing at a file outside: writing either used to
    /// follow the link and overwrite that file.
    #[test]
    fn a_link_at_the_manifest_copy_or_stamp_writes_nothing_outside() {
        for name in [MANIFEST_COPY, STAMP] {
            let published = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let outside = published.path().join("shell-profile");
            fs::write(&outside, "kept\n").unwrap();
            let target = outside.to_str().unwrap();
            let url = publish_with(
                published.path(),
                "#!/bin/sh\n",
                &[Link::Symbolic(name, target)],
            );
            let found = lookup(&url).unwrap();
            let e = format!(
                "{:#}",
                ensure(root.path(), &found.url, &found.sha256).unwrap_err()
            );
            assert!(e.contains(name), "{name}: {e}");
            assert_eq!(fs::read_to_string(&outside).unwrap(), "kept\n", "{name}");
            assert!(installed(root.path(), &found.sha256).is_none());
        }
    }

    /// The same names linked to a file inside the package: the link is
    /// replaced by Datalib's own file, and the package's file is left
    /// as the archive had it.
    #[test]
    fn a_link_at_the_stamp_inside_the_package_is_replaced_not_followed() {
        let published = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho hi\n";
        let url = publish_with(
            published.path(),
            script,
            &[
                Link::Symbolic(MANIFEST_COPY, "bin/fetch-weather"),
                Link::Symbolic(STAMP, "bin/fetch-weather"),
            ],
        );
        let found = lookup(&url).unwrap();
        ensure(root.path(), &found.url, &found.sha256).unwrap();
        let dir = package_dir(root.path(), &found.sha256);
        assert_eq!(
            fs::read_to_string(dir.join("bin/fetch-weather")).unwrap(),
            script
        );
        for name in [MANIFEST_COPY, STAMP] {
            assert!(
                fs::symlink_metadata(dir.join(name)).unwrap().is_file(),
                "{name}"
            );
        }
        assert!(installed(root.path(), &found.sha256).is_some());
    }

    /// A symlink or hard link reaching outside the package is refused,
    /// and nothing is installed.
    #[test]
    fn a_link_out_of_the_package_is_refused() {
        let published = tempfile::tempdir().unwrap();
        let outside = published.path().join("secret");
        fs::write(&outside, "x").unwrap();
        let outside = outside.to_str().unwrap().to_string();
        for link in [
            Link::Symbolic("bin/sh", "/bin/sh"),
            Link::Symbolic("bin/up", "../../../../../../../../secret"),
            Link::Hard("bin/hard", &outside),
            Link::Hard("bin/hard", "../../../../../../../../secret"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let url = publish_with(published.path(), "#!/bin/sh\n", &[link]);
            let found = lookup(&url).unwrap();
            assert!(ensure(root.path(), &found.url, &found.sha256).is_err());
            assert!(installed(root.path(), &found.sha256).is_none());
            assert!(!package_dir(root.path(), &found.sha256).exists());
        }
    }

    /// The build the manifest names is pinned by its sha256: bytes that
    /// do not hash to it are refused even though the manifest itself
    /// is unchanged.
    #[test]
    fn a_build_that_does_not_match_the_manifest_is_refused() {
        let published = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let url = publish(published.path(), "#!/bin/sh\necho hi\n");
        let found = lookup(&url).unwrap();
        write_build(
            &published.path().join("weather.tar.gz"),
            "#!/bin/sh\necho swapped\n",
            &[],
        );
        let e = format!(
            "{:#}",
            ensure(root.path(), &found.url, &found.sha256).unwrap_err()
        );
        assert!(e.contains("the manifest says"), "{e}");
        assert!(installed(root.path(), &found.sha256).is_none());
    }

    /// The app fetches every configured package when the Manage screen
    /// lists them, and the add flow fetches one too, so two `ensure`s of
    /// one package can run at once; each used to remove the directory
    /// the other had just moved into place.
    #[test]
    fn two_ensures_of_one_package_both_succeed() {
        let published = tempfile::tempdir().unwrap();
        let url = publish(published.path(), "#!/bin/sh\necho hi\n");
        let found = lookup(&url).unwrap();
        for _ in 0..20 {
            let root = tempfile::tempdir().unwrap();
            let gate = std::sync::Barrier::new(4);
            std::thread::scope(|s| {
                let runs: Vec<_> = (0..4)
                    .map(|_| {
                        s.spawn(|| {
                            gate.wait();
                            ensure(root.path(), &found.url, &found.sha256).map(|_| ())
                        })
                    })
                    .collect();
                for run in runs {
                    run.join().unwrap().unwrap();
                }
            });
            assert!(installed(root.path(), &found.sha256).is_some());
        }
    }
}
