//! A release tarball without `runtime/` fetches the runtime its manifest
//! names on first use, verifies it, and refuses one that hashes wrong.
//!
//! The shape of a tarball install is rebuilt in a temp dir: the real
//! `datalib-step` beside a `runtime.manifest`, no `runtime/`, a local
//! HTTP server standing in for the GitHub release, and a stand-in
//! runtime whose `node` is a shell script that echoes what it was asked
//! to run. `datalib-step pull-runtime` resolves qmd through the same
//! path a sync takes, so what it reports is what a sync would run.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use datalib_runtime::node_runtime::{AssetKind, Manifest, RuntimeAsset, MANIFEST_FILE};
use datalib_runtime::qmd::DEFAULT_QMD_VERSION;
use sha2::{Digest, Sha256};

/// A tarball-shaped install: the binary copied (not linked — the
/// resolver canonicalises its own path before looking beside it) into
/// a directory of its own, plus whatever manifest the test writes.
struct Install {
    dir: PathBuf,
}

impl Install {
    fn new(base: &Path) -> Self {
        let dir = base.join("datalib-0.0.0-test");
        fs::create_dir_all(&dir).unwrap();
        let src = PathBuf::from(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"));
        let bin = dir.join("datalib-step");
        fs::copy(&src, &bin).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn write_manifest(&self, manifest: &Manifest) {
        fs::write(self.dir.join(MANIFEST_FILE), manifest.render()).unwrap();
    }

    fn pull_runtime(&self, cache_home: &Path) -> std::process::Output {
        Command::new(self.dir.join("datalib-step"))
            .arg("pull-runtime")
            .env("XDG_CACHE_HOME", cache_home)
            .env_remove("DATALIB_RUNTIME_DIR")
            .env_remove("DATALIB_ALLOW_NPX")
            .output()
            .unwrap()
    }
}

/// A `.tar.gz` of a runtime whose `node` echoes its arguments and whose
/// qmd entry exists, so `qmd --version` through the resolver prints
/// the path it resolved to.
fn fake_runtime_tarball(base: &Path) -> Vec<u8> {
    let src = base.join("runtime-src");
    let node = src.join("node/bin/node");
    fs::create_dir_all(node.parent().unwrap()).unwrap();
    fs::write(&node, "#!/bin/sh\necho \"fake-node $*\"\n").unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let entry = src.join(format!(
        "qmd/{DEFAULT_QMD_VERSION}/node_modules/@tobilu/qmd/dist/cli/qmd.js"
    ));
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "// qmd\n").unwrap();

    let mut buf = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all(".", &src).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    buf
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Serves one body at every path, counting requests. Just enough HTTP
/// for reqwest: read the request head, answer with a length and close.
fn serve(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    });
    (format!("http://127.0.0.1:{port}"), hits)
}

fn manifest_for(url_base: &str, body_len: usize, sha256: &str) -> Manifest {
    Manifest::new(
        RuntimeAsset {
            kind: AssetKind::Cpu,
            name: "runtime-test.tar.gz".to_string(),
            sha256: sha256.to_string(),
            bytes: body_len as u64,
            url: format!("{url_base}/runtime-test.tar.gz"),
        },
        None,
    )
}

fn runtime_dirs(cache_home: &Path) -> Vec<String> {
    let cache = cache_home.join("datalib").join("runtime");
    let Ok(entries) = fs::read_dir(&cache) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".lock" && n != "CACHEDIR.TAG")
        .collect();
    names.sort();
    names
}

/// The first use fetches, verifies and unpacks; qmd then resolves out
/// of the cache; the second use finds it there and fetches nothing.
#[test]
fn first_use_fetches_the_manifests_runtime_and_the_second_does_not() {
    let td = tempfile::tempdir().unwrap();
    let install = Install::new(td.path());
    let body = fake_runtime_tarball(td.path());
    let sha = sha256_hex(&body);
    let (url_base, hits) = serve(body.clone());
    install.write_manifest(&manifest_for(&url_base, body.len(), &sha));
    let cache_home = td.path().join("cache");

    let out = install.pull_runtime(&cache_home);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "pull-runtime failed:\n{stderr}");
    let expected_root = cache_home.join("datalib/runtime").join(&sha[..12]);
    assert!(
        expected_root.join("node/bin/node").is_file(),
        "no node under {}:\n{stderr}",
        expected_root.display()
    );
    assert!(
        stderr.contains(&format!("runtime: {}", expected_root.display())),
        "{stderr}"
    );
    // The fake node echoed the entry it was given: qmd resolved from
    // the fetched tree, at the pinned version.
    assert!(
        stderr.contains(&format!(
            "qmd --version: fake-node {}/qmd/{DEFAULT_QMD_VERSION}/node_modules/@tobilu/qmd/dist/cli/qmd.js --version",
            expected_root.display()
        )),
        "{stderr}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(runtime_dirs(&cache_home), vec![sha[..12].to_string()]);
    assert!(cache_home.join("datalib/runtime/CACHEDIR.TAG").is_file());

    let out = install.pull_runtime(&cache_home);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the second use must not fetch"
    );
}

/// Bytes that hash wrong are refused: the run fails, says so, and
/// leaves no runtime behind — not a tree, not a partial download.
#[test]
fn a_wrong_hash_is_refused_and_nothing_is_unpacked() {
    let td = tempfile::tempdir().unwrap();
    let install = Install::new(td.path());
    let body = fake_runtime_tarball(td.path());
    let (url_base, _hits) = serve(body.clone());
    let wrong = "0".repeat(64);
    install.write_manifest(&manifest_for(&url_base, body.len(), &wrong));
    let cache_home = td.path().join("cache");

    let out = install.pull_runtime(&cache_home);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a wrong hash must fail:\n{stderr}");
    assert!(stderr.contains("sha256"), "{stderr}");
    assert!(stderr.contains("expected 0000"), "{stderr}");
    assert!(
        runtime_dirs(&cache_home).is_empty(),
        "cache holds {:?}",
        runtime_dirs(&cache_home)
    );
}

/// No manifest beside the binary means no fetch and a miss that says
/// so — a checkout build stays exactly as it was.
#[test]
fn no_manifest_means_no_fetch() {
    let td = tempfile::tempdir().unwrap();
    let install = Install::new(td.path());
    let cache_home = td.path().join("cache");
    let out = install.pull_runtime(&cache_home);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(MANIFEST_FILE), "{stderr}");
    assert!(!cache_home.exists());
}
