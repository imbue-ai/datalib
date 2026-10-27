//! What a person types to name a package, and where its manifest is.

use anyhow::{bail, Result};

/// The manifest's file name in a GitHub release.
pub const MANIFEST_FILE: &str = "datalib-package.toml";

/// The manifest URL `source` names:
///
/// - `github:owner/repo` — the latest release's `datalib-package.toml`;
/// - `github:owner/repo@tag` — that release's;
/// - an `https://` URL, or a `file://` one for a package being developed.
pub fn manifest_url(source: &str) -> Result<String> {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("github:") {
        let (repo, tag) = match rest.split_once('@') {
            Some((repo, tag)) => (repo, Some(tag)),
            None => (rest, None),
        };
        let parts: Vec<&str> = repo.split('/').collect();
        let ok = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if parts.len() != 2 || !parts.iter().all(|p| ok(p)) || tag.is_some_and(|t| !ok(t)) {
            bail!("{source:?} is not `github:owner/repo` or `github:owner/repo@tag`");
        }
        return Ok(match tag {
            Some(tag) => {
                format!("https://github.com/{repo}/releases/download/{tag}/{MANIFEST_FILE}")
            }
            None => format!("https://github.com/{repo}/releases/latest/download/{MANIFEST_FILE}"),
        });
    }
    if source.starts_with("https://") || source.starts_with("file:///") {
        return Ok(source.to_string());
    }
    bail!(
        "{source:?} is not a package: give `github:owner/repo`, or the https link to a \
         {MANIFEST_FILE}"
    )
}

/// An artifact's URL, which the manifest may write relative to itself.
pub fn join(manifest_url: &str, artifact: &str) -> Result<String> {
    if artifact.contains("://") {
        return Ok(artifact.to_string());
    }
    if artifact.starts_with('/') || artifact.split('/').any(|s| s == "..") {
        bail!("artifact url {artifact:?}: write it relative to the manifest, without `/` or `..`");
    }
    let base = match manifest_url.rfind('/') {
        Some(i) => &manifest_url[..=i],
        None => bail!("manifest url {manifest_url:?} has no directory"),
    };
    Ok(format!("{base}{artifact}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_shorthand() {
        assert_eq!(
            manifest_url("github:alice/datalib-weather").unwrap(),
            "https://github.com/alice/datalib-weather/releases/latest/download/datalib-package.toml"
        );
        assert_eq!(
            manifest_url("github:alice/datalib-weather@v1.2.0").unwrap(),
            "https://github.com/alice/datalib-weather/releases/download/v1.2.0/datalib-package.toml"
        );
        assert!(manifest_url("github:alice").is_err());
        assert!(manifest_url("github:alice/x/y").is_err());
        assert!(manifest_url("http://example.org/x.toml").is_err());
    }

    #[test]
    fn artifacts_resolve_against_the_manifest() {
        let m = "https://h/releases/download/v1/datalib-package.toml";
        assert_eq!(
            join(m, "w.tar.gz").unwrap(),
            "https://h/releases/download/v1/w.tar.gz"
        );
        assert_eq!(join(m, "https://o/w.tar.gz").unwrap(), "https://o/w.tar.gz");
        assert!(join(m, "../w.tar.gz").is_err());
        assert!(join(m, "/w.tar.gz").is_err());
    }
}
