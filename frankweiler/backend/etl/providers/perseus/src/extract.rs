//! Download configured TEI XML files from `PerseusDL/canonical-greekLit`
//! (master branch) to `<input_path>/<basename>`.
//!
//! There's no auth, no rate limit, no pagination, no incrementality —
//! every entry in [`PerseusSync::files`](frankweiler_core::config::PerseusSync)
//! is one GET to raw.githubusercontent.com. We deliberately bypass
//! the shared `latchkey_curl` HTTP layer here: that layer's value-add
//! is credential injection + per-host rate limiting + playback
//! fixtures, none of which apply to public-URL static-file fetches,
//! and requiring `latchkey services register …` + a dummy `auth set`
//! header on the user just to satisfy latchkey's "creds must be
//! set" check would be all-cost-no-benefit ceremony.
//!
//! Instead we shell out to `curl` directly via `tokio::process`.
//! `curl` is present on every reasonable dev host (macOS built-in,
//! standard Linux installs, the project devcontainer, every CI
//! runner we use). If we ever need playback fixtures here, the
//! shell-out is one function and easy to swap.
//!
//! The translate path under [`crate::translate`] currently expects
//! the **Thucydides Histories** pair specifically — `perseus-grc2.xml`
//! + `1st1K-eng1.xml`. The default [`PerseusSync::files`] list
//! ([`DEFAULT_FILES`]) matches that, so an empty / `sync: {}` block
//! does the right thing. If you point `files:` at a different work,
//! Extract will happily fetch it but Translate will fail to find the
//! basenames it expects — multi-work translate is a follow-up.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::instrument;

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;

/// `raw.githubusercontent.com` URL prefix for the canonical-greekLit
/// `data/` tree. Subpaths in [`PerseusSync::files`] are appended
/// verbatim, including any leading directory components.
pub const RAW_GITHUB_BASE: &str =
    "https://raw.githubusercontent.com/PerseusDL/canonical-greekLit/refs/heads/master/data";

/// Default fetch list when `sync.files` is empty/omitted. Matches the
/// two TEI editions the translate path expects:
///   * Greek: `tlg0003/tlg001/tlg0003.tlg001.perseus-grc2.xml`
///   * English: `tlg0003/tlg001/tlg0003.tlg001.1st1K-eng1.xml`
pub const DEFAULT_FILES: &[&str] = &[
    "tlg0003/tlg001/tlg0003.tlg001.perseus-grc2.xml",
    "tlg0003/tlg001/tlg0003.tlg001.1st1K-eng1.xml",
];

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Directory the basenames land in. Matches the source's
    /// resolved `input_path` so Translate finds them on the same
    /// path on the next phase.
    pub out_dir: PathBuf,
    /// Subpaths under `RAW_GITHUB_BASE`. Empty falls back to
    /// [`DEFAULT_FILES`].
    pub files: Vec<String>,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub requests: u64,
}

#[instrument(skip_all, fields(out_dir = %opts.out_dir.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let files: Vec<String> = if opts.files.is_empty() {
        DEFAULT_FILES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.files.clone()
    };

    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("mkdir -p {}", opts.out_dir.display()))?;

    if opts.control.reset_and_redownload {
        clear_xml_files(&opts.out_dir)?;
    }

    opts.progress.set_length(Some(files.len() as u64));
    let mut summary = FetchSummary::default();

    for subpath in &files {
        let basename = basename(subpath).ok_or_else(|| {
            anyhow::anyhow!("perseus `files` entry has no basename: {subpath:?}")
        })?;
        let dest = opts.out_dir.join(basename);
        let url = format!("{RAW_GITHUB_BASE}/{subpath}");
        opts.progress.set_message(&format!("perseus: {subpath}"));

        summary.requests += 1;
        curl_to_file(&url, &dest)
            .await
            .with_context(|| format!("GET {url}"))?;
        let bytes = std::fs::metadata(&dest)
            .with_context(|| format!("stat {}", dest.display()))?
            .len();
        summary.fetched += 1;
        summary.bytes += bytes;
        opts.progress.inc(1);
    }
    Ok(summary)
}

/// `curl -sSfL -o <dest> <url>`. `-f` makes curl exit non-zero on
/// HTTP 4xx/5xx so we don't write a "404 Not Found" body to disk and
/// hand it to the parser. `-L` follows GitHub's redirects (raw.* is
/// stable today but the redirect surface has changed before). `-S`
/// keeps error messages on stderr in silent mode so failures surface
/// cleanly in the run summary.
async fn curl_to_file(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("curl")
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
        anyhow::bail!(
            "curl {url}: exit {}: {}",
            status.status,
            stderr.trim()
        );
    }
    Ok(())
}

fn clear_xml_files(dir: &Path) -> Result<()> {
    // Wipe every `.xml` in the target dir so a follow-up Translate
    // sees a clean state. We only touch `.xml` to avoid blowing
    // away a sibling subdirectory or a user-staged playback fixture.
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("readdir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("xml") {
            std::fs::remove_file(&path)
                .with_context(|| format!("rm {}", path.display()))?;
        }
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

    #[test]
    fn default_files_match_translate_expected_basenames() {
        use crate::translate::{ENG_FILENAME, GRC_FILENAME};
        // Spine-of-the-pipeline assertion: if these drift, a default
        // `sync: {}` block leaves Translate looking for files Extract
        // never wrote.
        let default_basenames: Vec<&str> = DEFAULT_FILES
            .iter()
            .map(|s| basename(s).unwrap())
            .collect();
        assert!(default_basenames.contains(&GRC_FILENAME));
        assert!(default_basenames.contains(&ENG_FILENAME));
    }

    #[test]
    fn clear_xml_only_removes_xml_files() {
        let tmp = tempfile::tempdir().unwrap();
        let xml = tmp.path().join("a.xml");
        let other = tmp.path().join("b.txt");
        let sub = tmp.path().join("sub");
        std::fs::write(&xml, b"x").unwrap();
        std::fs::write(&other, b"y").unwrap();
        std::fs::create_dir(&sub).unwrap();
        clear_xml_files(tmp.path()).unwrap();
        assert!(!xml.exists());
        assert!(other.exists());
        assert!(sub.exists());
    }
}
