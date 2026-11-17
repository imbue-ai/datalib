//! Pinned first-use fetches. The caller knows the sha256 before the
//! bytes are fetched; a download that hashes wrong is refused and
//! nothing of it is kept. Users: the pinned GGUF models
//! (`datalib_embedding_models`), the perseus embedder's model files, the
//! Node runtime a release tarball names in its manifest ([`runtime`]),
//! and packages (`datalib_packages`), whose manifest is the one document
//! fetched unpinned ([`get_unpinned`]) so its hash can be shown and pinned.
//! An install that ships what it runs sets [`NO_FETCH_ENV`], and every
//! fetch here then fails instead of going to the network.

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

/// Set, to anything but empty or `0`, by an install that ships everything
/// it runs (the docker image): every fetch here is then refused, naming
/// what it wanted, so a missing piece is an error rather than a download
/// nobody asked for.
pub const NO_FETCH_ENV: &str = "DATALIB_NO_FETCH";

/// The refusal [`NO_FETCH_ENV`]'s value calls for, if any, for `url`.
fn refusal(switch: Option<&str>, url: &str) -> Option<anyhow::Error> {
    let on = switch.is_some_and(|v| !v.is_empty() && v != "0");
    on.then(|| {
        anyhow::anyhow!(
            "{url} is not here and this install fetches nothing ({NO_FETCH_ENV} is set): \
             it ships what it runs, so this one is missing from it. Set {NO_FETCH_ENV}=0 \
             to let it be fetched"
        )
    })
}

fn allowed(url: &str) -> Result<()> {
    match refusal(std::env::var(NO_FETCH_ENV).ok().as_deref(), url) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

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
    allowed(url)?;
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

/// GET `url` to a partial file beside `dest`, hashing as it streams;
/// rename into place only on a matching digest. Any number of processes
/// may fetch one `dest` at once: each writes its own partial file, and the
/// rename replaces whatever an earlier one put there with the same bytes.
/// A partial file left by a fetch that was killed is removed first.
/// `tag` prefixes the status lines (`[models]`, `[runtime]`).
/// HuggingFace has answered a cold fetch with 429 before, so a retryable
/// status backs off and tries again rather than failing the sync on the
/// first refusal.
pub fn download_verified(tag: &str, url: &str, want: &str, dest: &Path) -> Result<()> {
    allowed(url)?;
    sweep_stale_partials(tag, dest);
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
    let resp = client
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
    write_verified(url, resp, want, dest)
}

/// Stream `body` to a partial file of this fetch's own, hashing as it
/// goes, and rename it over `dest` only when the digest is `want`. A
/// partial file that is not renamed is removed when it drops.
fn write_verified(
    url: &str,
    mut body: impl Read,
    want: &str,
    dest: &Path,
) -> std::result::Result<(), FetchError> {
    let mut file = partial_file(dest)?;
    let partial = file.path().to_path_buf();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = body
            .read(&mut buf)
            .map_err(|e| Retry(format!("read failed mid-body: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("write {}", partial.display()))?;
    }
    file.as_file()
        .sync_all()
        .with_context(|| format!("sync {}", partial.display()))?;
    let got = hex(&hasher.finalize());
    if got != want {
        return Err(Fatal(anyhow::anyhow!(
            "{url}: sha256 {got}, expected {want} — the pinned source no longer serves \
             the pinned bytes, or something rewrote the download; nothing was kept"
        )));
    }
    file.persist(dest)
        .map_err(|e| anyhow::Error::new(e.error))
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(())
}

const PARTIAL_SUFFIX: &str = ".partial";

/// A partial file beside `dest` that no other fetch writes:
/// `<dest name>.<random>.partial`. Two processes fetching one `dest` at
/// once (parallel tests sharing one model cache) each need their own; a
/// shared name let the second's create truncate the first's, and the
/// slower rename then found nothing to move.
fn partial_file(dest: &Path) -> Result<tempfile::NamedTempFile> {
    let dir = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut prefix = dest.file_name().unwrap_or_default().to_os_string();
    prefix.push(".");
    tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(PARTIAL_SUFFIX)
        // What `File::create` asks for, so the umask decides the mode and
        // the model is as readable as it was when the name was fixed.
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .tempfile_in(dir)
        .with_context(|| {
            format!(
                "create a partial file for {} in {}",
                dest.display(),
                dir.display()
            )
        })
}

/// A partial file nobody has written to for this long belongs to a fetch
/// that was killed: a live one touches its file with every chunk.
const STALE_PARTIAL: Duration = Duration::from_secs(60 * 60);

/// Which of `entries` (name, mtime) are partial files of `dest_name` left
/// by a killed fetch, so not every interrupted download leaves its bytes
/// behind for good. `<dest_name>.partial`, the one name older builds
/// used, counts too.
fn stale_partials(
    dest_name: &std::ffi::OsStr,
    entries: &[(std::ffi::OsString, std::time::SystemTime)],
    now: std::time::SystemTime,
) -> Vec<std::ffi::OsString> {
    let mut prefix = dest_name.to_os_string();
    prefix.push(".");
    let prefix = prefix.to_string_lossy();
    entries
        .iter()
        .filter(|(name, mtime)| {
            let name = name.to_string_lossy();
            let ours = name.starts_with(prefix.as_ref()) && name.ends_with(PARTIAL_SUFFIX);
            let idle = now.duration_since(*mtime).unwrap_or_default();
            ours && idle >= STALE_PARTIAL
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Remove the partial files of `dest` that [`stale_partials`] picks, each
/// said on a status line. One that cannot be removed is said and left.
fn sweep_stale_partials(tag: &str, dest: &Path) {
    let (Some(dir), Some(dest_name)) = (dest.parent(), dest.file_name()) else {
        return;
    };
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<_> = listing
        .filter_map(|e| {
            let e = e.ok()?;
            Some((e.file_name(), e.metadata().ok()?.modified().ok()?))
        })
        .collect();
    for name in stale_partials(dest_name, &entries, std::time::SystemTime::now()) {
        let path = dir.join(&name);
        match fs::remove_file(&path) {
            Ok(()) => status_line!(
                "{tag} removed {}, left by a fetch that stopped",
                path.display()
            ),
            Err(e) => status_line!("{tag} could not remove {}: {e}", path.display()),
        }
    }
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

    /// The docker image sets the switch, so anything it lacks is refused
    /// by name rather than downloaded at its first start.
    #[test]
    fn the_no_fetch_switch_refuses_unless_it_is_empty_or_zero() {
        let url = "https://huggingface.co/x/resolve/abc/model.gguf";
        for off in [None, Some(""), Some("0")] {
            assert!(refusal(off, url).is_none(), "{off:?}");
        }
        for on in ["1", "true", "yes"] {
            let e = refusal(Some(on), url).expect(on).to_string();
            assert!(e.contains(url) && e.contains(NO_FETCH_ENV), "{e}");
        }
    }

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

    /// A body that waits at `gate` before giving up its bytes. Its first
    /// read comes after the writer has opened its partial file, so the
    /// gate holds every writer there until all of them have.
    struct Gated<'a> {
        gate: &'a std::sync::Barrier,
        bytes: &'a [u8],
        waited: bool,
    }

    impl Read for Gated<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.waited {
                self.waited = true;
                self.gate.wait();
            }
            let n = self.bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }

    /// Two processes fetching the same model into a shared cache, as
    /// parallel tests under one `$HOME` do: both partial files exist at
    /// once, and both fetches must end with the file in place. One fixed
    /// `<dest>.partial` made the second rename fail with ENOENT.
    #[test]
    fn two_writers_of_one_dest_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let gate = std::sync::Barrier::new(2);
        let outcomes: Vec<std::result::Result<(), String>> = std::thread::scope(|s| {
            let writers: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        let body = Gated {
                            gate: &gate,
                            bytes: b"hello",
                            waited: false,
                        };
                        write_verified("test://model", body, HELLO_SHA, &dest).map_err(
                            |e| match e {
                                Retry(r) => r,
                                Fatal(e) => format!("{e:#}"),
                            },
                        )
                    })
                })
                .collect();
            writers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert_eq!(outcomes, [Ok(()), Ok(())]);
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["model.gguf"], "no partial file is left behind");
    }

    /// The extension is kept whole (`runtime-x.tar.gz.…`, not
    /// `runtime-x.tar.…`), each fetch gets a name of its own, and the
    /// file is made with the mode `File::create` gives.
    #[test]
    fn a_partial_file_keeps_the_whole_name_and_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("runtime-x.tar.gz");
        let (a, b) = (partial_file(&dest).unwrap(), partial_file(&dest).unwrap());
        for p in [a.path(), b.path()] {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with("runtime-x.tar.gz.") && name.ends_with(".partial"),
                "{name}"
            );
            assert_eq!(p.parent(), Some(dir.path()));
        }
        assert_ne!(a.path(), b.path());

        // Not tempfile's owner-only 0600: a model fetched while building
        // an image is read by whoever the container runs as.
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode();
        let created = dir.path().join("created");
        fs::File::create(&created).unwrap();
        assert_eq!(mode(a.path()), mode(&created));
    }

    /// A killed fetch's partial file goes once it has sat idle; a live
    /// one, another dest's, and the dest itself stay.
    #[test]
    fn only_idle_partials_of_this_dest_are_swept() {
        use std::ffi::{OsStr, OsString};
        let now = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let idle = now - STALE_PARTIAL;
        let live = now - Duration::from_secs(5);
        let entries: Vec<(OsString, _)> = [
            ("m.gguf", idle),
            ("m.gguf.partial", idle),
            ("m.gguf.Ab12Cd.partial", idle),
            ("m.gguf.Ef34Gh.partial", live),
            ("m.gguf.sha256-ok", idle),
            ("other.gguf.Ab12Cd.partial", idle),
            ("m.ggu.partial", idle),
        ]
        .into_iter()
        .map(|(n, t)| (n.into(), t))
        .collect();
        assert_eq!(
            stale_partials(OsStr::new("m.gguf"), &entries, now),
            ["m.gguf.partial", "m.gguf.Ab12Cd.partial"]
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
