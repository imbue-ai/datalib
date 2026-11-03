//! What a person types to name a package, and where its manifest is.

use anyhow::{bail, Result};
use url::Url;

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
    match manifest(source) {
        Ok(url) => Ok(url.into()),
        Err(_) => bail!(
            "{source:?} is not a package: give `github:owner/repo`, or the https link to a \
             {MANIFEST_FILE}"
        ),
    }
}

/// A manifest's URL, as one a package may be fetched from: `https`, or
/// a `file` on this machine for a package being developed.
pub fn manifest(url: &str) -> Result<Url> {
    let parsed = Url::parse(url)?;
    match parsed.scheme() {
        "https" if parsed.host().is_some() => Ok(parsed),
        "file" => Ok(parsed),
        _ => bail!("manifest url {url:?}: a package is fetched over https, or from a file:// path"),
    }
}

/// An artifact's URL, which the manifest may write relative to itself.
/// A relative one stays in the manifest's directory once parsed; an
/// absolute one is `https`, or a `file` only beside a `file` manifest,
/// so a published manifest cannot name a file on this machine.
pub fn join(manifest_url: &str, artifact: &str) -> Result<Url> {
    let base = manifest(manifest_url)?;
    let dir = base.join("./")?;
    let joined = base.join(artifact)?;
    let relative = Url::parse(artifact).is_err();
    let ok = if relative {
        joined.as_str().starts_with(dir.as_str())
    } else {
        (joined.scheme() == "https" && joined.host().is_some())
            || (joined.scheme() == "file" && base.scheme() == "file")
    };
    if !ok {
        bail!(
            "artifact url {artifact:?}: write it relative to the manifest, inside its \
             directory, or as an https link"
        );
    }
    Ok(joined)
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
        assert!(manifest_url("ftp://example.org/x.toml").is_err());
        assert_eq!(
            manifest_url("file:///pkg/datalib-package.toml").unwrap(),
            "file:///pkg/datalib-package.toml"
        );
    }

    #[test]
    fn artifacts_resolve_against_the_manifest() {
        let m = "https://h/releases/download/v1/datalib-package.toml";
        let joined = |a: &str| join(m, a).map(String::from);
        assert_eq!(
            joined("w.tar.gz").unwrap(),
            "https://h/releases/download/v1/w.tar.gz"
        );
        assert_eq!(joined("https://o/w.tar.gz").unwrap(), "https://o/w.tar.gz");
        assert!(joined("../w.tar.gz").is_err());
        assert!(joined("/w.tar.gz").is_err());
    }

    /// What climbs or leaves once parsed is refused, not only what is
    /// spelled `..`; and a published manifest cannot name a file here
    /// or send the build over http.
    #[test]
    fn an_artifact_cannot_leave_the_manifests_directory_or_https() {
        let m = "https://h/releases/download/v1/datalib-package.toml";
        for artifact in [
            "%2e%2e/w.tar.gz",
            "%2E%2E/%2e%2e/w.tar.gz",
            "..\\w.tar.gz",
            ".%2e/w.tar.gz",
            "//evil.example/w.tar.gz",
            "\\\\evil.example/w.tar.gz",
            "http://h/w.tar.gz",
            "file:///etc/passwd",
            "ftp://h/w.tar.gz",
        ] {
            assert!(join(m, artifact).is_err(), "{artifact}");
        }
        let local = "file:///pkg/datalib-package.toml";
        assert_eq!(
            String::from(join(local, "w.tar.gz").unwrap()),
            "file:///pkg/w.tar.gz"
        );
        assert!(join(local, "file:///elsewhere/w.tar.gz").is_ok());
        assert!(join("http://h/datalib-package.toml", "w.tar.gz").is_err());
    }
}
