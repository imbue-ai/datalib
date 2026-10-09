//! Download configured TEI XML files from `PerseusDL/canonical-greekLit`
//! (master branch) to `<input_path>/<basename>`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::instrument;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;

/// `raw.githubusercontent.com` URL prefix for the canonical-greekLit
/// `data/` tree. Subpaths in [`PerseusSync::files`] are appended
/// verbatim, including any leading directory components.
pub const RAW_GITHUB_BASE: &str =
    "https://raw.githubusercontent.com/PerseusDL/canonical-greekLit/refs/heads/master/data";

/// Default fetch list when `sync.files` is empty/omitted: every
/// published edition/translation of Thucydides' Histories in
/// PerseusDL's `tlg0003/tlg001/` tree, plus `__cts__.xml` (the CTS
/// metadata the render path reads for human-readable edition
/// titles). The Greek original (`perseus-grc2`) sits first.
pub const DEFAULT_FILES: &[&str] = &[
    "tlg0003/tlg001/__cts__.xml",
    "tlg0003/tlg001/tlg0003.tlg001.perseus-grc2.xml",
    "tlg0003/tlg001/tlg0003.tlg001.perseus-eng4.xml",
    "tlg0003/tlg001/tlg0003.tlg001.perseus-eng6.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-eng1.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-eng2.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-fre1.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-fre2.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-ger1.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-ger2.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-ger3.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-ger4.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-ita1.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-lat2.xml",
];

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Directory the basenames land in. Matches the source's
    /// resolved `input_path` so Render finds them on the same
    /// path on the next phase.
    pub out_dir: PathBuf,
    /// Subpaths under `RAW_GITHUB_BASE`. Empty falls back to
    /// [`DEFAULT_FILES`].
    pub files: Vec<String>,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub requests: u64,
}

/// Perseus keeps no store, so a file that would not download can be no
/// `problems` row: every file is tried, the ones that came keep their
/// new bytes, and the run fails at the end naming each one that did not.
#[instrument(skip_all, fields(out_dir = %opts.out_dir.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    fetch_from(RAW_GITHUB_BASE, "curl", opts).await
}

async fn fetch_from(base: &str, curl: &str, opts: FetchOptions) -> Result<FetchSummary> {
    let files: Vec<String> = if opts.files.is_empty() {
        DEFAULT_FILES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.files.clone()
    };

    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("mkdir -p {}", opts.out_dir.display()))?;

    opts.progress.set_length(Some(files.len() as u64));
    let mut summary = FetchSummary::default();
    let mut failed: Vec<String> = Vec::new();

    for subpath in &files {
        opts.progress.inc(1);
        let Some(basename) = basename(subpath) else {
            failed.push(format!("{subpath:?}: the entry has no basename"));
            continue;
        };
        let dest = opts.out_dir.join(basename);
        let url = format!("{base}/{subpath}");
        opts.progress.set_message(&format!("perseus: {subpath}"));

        summary.requests += 1;
        if let Err(e) = curl_to_file(curl, &url, &dest).await {
            failed.push(format!("{subpath}: {e:#}"));
            continue;
        }
        let bytes = std::fs::metadata(&dest)
            .with_context(|| format!("stat {}", dest.display()))?
            .len();
        summary.fetched += 1;
        summary.bytes += bytes;
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "{} of {} perseus files did not download (the rest did, and any \
             earlier copy of these is left as it was):\n{}",
            failed.len(),
            files.len(),
            failed.join("\n")
        );
    }
    Ok(summary)
}

/// `curl -sSfL -o <dest> <url>`. `-f` makes curl exit non-zero on
/// HTTP 4xx/5xx so we don't write a "404 Not Found" body to disk and
/// hand it to the parser. `-L` follows GitHub's redirects (raw.* is
/// stable today but the redirect surface has changed before). `-S`
/// keeps error messages on stderr in silent mode so failures surface
/// cleanly in the run summary.
///
/// The transfer lands beside `dest` and is renamed over it only once
/// complete: `-o dest` truncates first, so a failed one would cost the
/// last good copy.
async fn curl_to_file(curl: &str, url: &str, dest: &Path) -> Result<()> {
    let part = part_path(dest);
    let result = run_curl(curl, url, &part).await.and_then(|()| {
        std::fs::rename(&part, dest)
            .with_context(|| format!("rename {} -> {}", part.display(), dest.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

async fn run_curl(curl: &str, url: &str, dest: &Path) -> Result<()> {
    let status = Command::new(curl)
        .arg("-sSfL")
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "spawn curl (is curl on PATH?)")?
        .wait_with_output()
        .await
        .with_context(|| "wait curl")?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr).into_owned();
        anyhow::bail!("curl {url}: exit {}: {}", status.status, stderr.trim());
    }
    Ok(())
}

fn basename(subpath: &str) -> Option<&str> {
    Path::new(subpath).file_name().and_then(|s| s.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_strips_directories() {
        assert_eq!(basename("a/b/c.xml"), Some("c.xml"));
        assert_eq!(basename("c.xml"), Some("c.xml"));
        assert_eq!(basename(""), None);
        assert_eq!(basename("/"), None);
    }

    /// A file that would not download costs that file: the ones after it
    /// are still fetched, the run fails naming it, and the copy an
    /// earlier run left of it is kept, though the transfer died having
    /// written half of it.
    #[tokio::test]
    async fn one_failed_file_costs_that_file_only() {
        let upstream = tempfile::tempdir().unwrap();
        for (name, text) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            std::fs::write(
                upstream.path().join(format!("{name}.xml")),
                format!("<TEI>{text}</TEI>"),
            )
            .unwrap();
        }
        // A curl whose transfer of b.xml dies partway, the way a dropped
        // connection does; the others are real curl over file://.
        let fake_curl = upstream.path().join("curl");
        std::fs::write(
            &fake_curl,
            "#!/bin/sh\n\
             case \"$4\" in *b.xml) printf '<TEI>be' > \"$3\"; \
             echo 'curl: (56) Recv failure' >&2; exit 56;; esac\n\
             exec curl \"$@\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("b.xml"), "<TEI>the last good beta</TEI>").unwrap();

        let base = format!("file://{}", upstream.path().display());
        let err = fetch_from(
            &base,
            &fake_curl.to_string_lossy(),
            FetchOptions {
                out_dir: out.path().to_path_buf(),
                files: vec!["a.xml".into(), "b.xml".into(), "c.xml".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("1 of 3") && msg.contains("b.xml: "), "{msg}");
        assert_eq!(
            std::fs::read_to_string(out.path().join("c.xml")).unwrap(),
            "<TEI>gamma</TEI>",
            "the file after the failure was still fetched"
        );
        assert_eq!(
            std::fs::read_to_string(out.path().join("b.xml")).unwrap(),
            "<TEI>the last good beta</TEI>",
            "a failed transfer does not truncate the last good copy"
        );
        assert!(!out.path().join("b.xml.part").exists());
    }

    #[test]
    fn default_files_include_grc_eng_and_cts() {
        // Spine-of-the-pipeline assertion: a default `sync: {}` block
        // must fetch the Greek original, at least one English
        // translation, and the CTS metadata the render path reads
        // for edition titles.
        let default_basenames: Vec<&str> =
            DEFAULT_FILES.iter().map(|s| basename(s).unwrap()).collect();
        assert!(default_basenames.contains(&"tlg0003.tlg001.perseus-grc2.xml"));
        assert!(default_basenames.contains(&"tlg0003.tlg001.1st1K-eng1.xml"));
        assert!(default_basenames.contains(&"__cts__.xml"));
    }
}
