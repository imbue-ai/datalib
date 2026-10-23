//! Pinned first-use fetches. The caller knows the sha256 before the
//! bytes are fetched; a download that hashes wrong is refused and
//! nothing of it is kept. Users: qmd's GGUF models
//! (`datalib_qmd_models`), the perseus embedder's model files, the
//! Node runtime a release tarball names in its manifest ([`runtime`]),
//! and packages (`datalib_packages`), whose manifest is the one document
//! fetched unpinned ([`get_unpinned`]) so its hash can be shown and pinned.

pub mod runtime;

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use datalib_obs::status_line;
use sha2::{Digest, Sha256};

const CHUNK: usize = 1 << 20;
const ATTEMPTS: u32 = 6;
const MAX_REDIRECTS: usize = 10;

pub use runtime::{enable_runtime_fetch, ensure_runtime, ReleaseRuntimeFetcher};

/// The TLS every HTTP client here is built with
/// (`ClientBuilder::tls_backend_preconfigured`): rustls on ring, trusting
/// webpki's bundled roots and nothing the host has installed.
pub fn tls_config() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring's provider supports rustls's default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
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

fn client() -> Result<reqwest::blocking::Client> {
    client_with(reqwest::redirect::Policy::default())
}

fn client_with(redirects: reqwest::redirect::Policy) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .redirect(redirects)
        .tls_backend_preconfigured(tls_config())
        .connect_timeout(Duration::from_secs(30))
        // The whole-request timeout would cap a multi-GB download; a
        // stalled body read is caught by the connection's own timeouts
        // and retried below.
        .timeout(None)
        .user_agent(concat!("datalib/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")
}

/// A small document fetched before its hash was known.
pub struct Unpinned {
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// Where the bytes came from after the redirects that stay on the
    /// requested host: GitHub's `releases/latest/download/…` gives the
    /// tagged release's URL, not the expiring CDN link it then sends to.
    pub final_url: String,
}

/// GET a small document whose hash is not known yet — a package manifest
/// being looked up — refusing one over `limit` bytes. A caller that goes
/// on to trust the bytes pins the hash this returns.
pub fn get_unpinned(url: &str, limit: u64) -> Result<Unpinned> {
    let hops = Arc::new(Mutex::new(Vec::new()));
    let seen = hops.clone();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        seen.lock().unwrap().push(attempt.url().clone());
        attempt.follow()
    });
    let requested = reqwest::Url::parse(url).with_context(|| format!("parse {url}"))?;
    let resp = client_with(policy)?
        .get(requested.clone())
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{url}: HTTP {status}");
    }
    let final_url = last_on_host(&requested, &hops.lock().unwrap()).to_string();
    let mut body = Vec::new();
    resp.take(limit + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("read {url}"))?;
    if body.len() as u64 > limit {
        bail!("{url}: larger than {limit} bytes");
    }
    Ok(Unpinned {
        sha256: sha256_bytes(&body),
        bytes: body,
        final_url,
    })
}

fn last_on_host<'a>(requested: &'a reqwest::Url, hops: &'a [reqwest::Url]) -> &'a reqwest::Url {
    hops.iter()
        .take_while(|u| u.host_str() == requested.host_str())
        .last()
        .unwrap_or(requested)
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// GET `url` to `<dest>.partial`, hashing as it streams; rename into
/// place only on a matching digest. `tag` prefixes the status lines
/// (`[qmd-models]`, `[runtime]`). HuggingFace has answered a cold fetch
/// with 429 before, so a retryable status backs off and tries again
/// rather than failing the sync on the first refusal.
pub fn download_verified(tag: &str, url: &str, want: &str, dest: &Path) -> Result<()> {
    let client = client()?;
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        match fetch_once(&client, url, want, dest) {
            Ok(()) => return Ok(()),
            Err(Retry(reason)) if attempt < ATTEMPTS => {
                status_line!(
                    "{tag} {url}: {reason}; retrying in {}s ({attempt}/{ATTEMPTS})",
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
    let partial = partial_path(dest);
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
            "{url}: sha256 {got}, expected {want} — the pinned source no longer serves \
             the pinned bytes, or something rewrote the download; nothing was kept"
        )));
    }
    fs::rename(&partial, dest)
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(())
}

/// `<dest>.partial`, appended rather than swapped for the extension so
/// `runtime-x.tar.gz` does not become `runtime-x.tar.partial`.
fn partial_path(dest: &Path) -> std::path::PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    dest.with_file_name(name)
}

/// Unpack a `.tar.gz` into `dest`, which must exist. Entries that would
/// land outside `dest` are refused by the tar crate; modes and symlinks
/// are kept, which the pnpm layout and `node/bin/node` both need.
pub fn unpack_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut tar = tar::Archive::new(gz);
    tar.set_overwrite(true);
    tar.unpack(dest)
        .with_context(|| format!("unpack {} into {}", archive.display(), dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn sha256_of_known_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), HELLO_SHA);
    }

    /// GitHub's `latest` link redirects to the tagged release on
    /// github.com and then to an expiring CDN URL; a package pins the
    /// tagged one.
    #[test]
    fn final_url_stops_where_the_redirects_leave_the_host() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        let requested = url("https://github.com/a/b/releases/latest/download/m.toml");
        let hops = [
            url("https://github.com/a/b/releases/download/v1/m.toml"),
            url("https://release-assets.githubusercontent.com/x?sig=1"),
        ];
        assert_eq!(last_on_host(&requested, &hops), &hops[0]);
        assert_eq!(last_on_host(&requested, &[]), &requested);
    }

    #[test]
    fn partial_keeps_the_whole_name() {
        assert_eq!(
            partial_path(Path::new("/c/runtime-x.tar.gz")),
            Path::new("/c/runtime-x.tar.gz.partial")
        );
    }

    /// Modes and symlinks survive the round trip — a runtime whose
    /// `node` lost its exec bit, or whose pnpm links dereferenced, is
    /// no runtime.
    #[test]
    fn unpack_keeps_modes_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("node/bin")).unwrap();
        fs::write(src.join("node/bin/node"), b"#!/bin/sh\necho node\n").unwrap();
        fs::set_permissions(src.join("node/bin/node"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("node/bin/node", src.join("link")).unwrap();

        let archive = dir.path().join("rt.tar.gz");
        {
            let f = fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            tar.follow_symlinks(false);
            tar.append_dir_all(".", &src).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }

        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        unpack_tar_gz(&archive, &out).unwrap();
        let node = out.join("node/bin/node");
        assert!(fs::metadata(&node).unwrap().permissions().mode() & 0o111 != 0);
        assert!(fs::symlink_metadata(out.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(out.join("link")).unwrap(),
            fs::read(&node).unwrap()
        );
    }
}
