//! The path a step writes, which is also the step's id.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A validated data-root-relative artifact path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactPath {
    raw: String,
}

impl ArtifactPath {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let raw = raw.trim_matches('/');
        if raw.is_empty() {
            anyhow::bail!("artifact path must be a non-empty relative path");
        }
        for s in raw.split('/') {
            match s {
                "" => anyhow::bail!("artifact path {raw:?} has an empty segment"),
                "." | ".." => anyhow::bail!("artifact path {raw:?} may not contain `.`/`..`"),
                _ => {}
            }
        }
        Ok(Self {
            raw: raw.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The first path segment. The stem a UI groups siblings by
    /// (`work-slack/raw` and `work-slack/rendered_md` share
    /// `work-slack`) — a display convenience, never how anything
    /// resolves identity.
    pub fn stem(&self) -> &str {
        self.raw.split('/').next().unwrap_or(&self.raw)
    }
}

impl fmt::Display for ArtifactPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for ArtifactPath {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for ArtifactPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        ArtifactPath::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_bad_paths() {
        assert!(ArtifactPath::parse("").is_err());
        assert!(ArtifactPath::parse("/").is_err());
        assert!(ArtifactPath::parse("a//b").is_err());
        assert!(ArtifactPath::parse("a/../b").is_err());
        assert!(ArtifactPath::parse("./a").is_err());
    }

    #[test]
    fn parse_trims_surrounding_slashes() {
        assert_eq!(ArtifactPath::parse("/a/b/").unwrap().as_str(), "a/b");
        assert_eq!(ArtifactPath::parse("a/b").unwrap().as_str(), "a/b");
    }

    /// A `*` is no longer a wildcard — it is a character, and one the
    /// config's id rules reject. Pinned so nobody reintroduces glob
    /// semantics here by accident.
    #[test]
    fn a_star_is_just_a_character() {
        assert_eq!(ArtifactPath::parse("a/*").unwrap().as_str(), "a/*");
        assert_eq!(ArtifactPath::parse("**/x").unwrap().as_str(), "**/x");
    }

    #[test]
    fn stem_is_the_first_segment() {
        assert_eq!(
            ArtifactPath::parse("work-slack/raw").unwrap().stem(),
            "work-slack"
        );
        assert_eq!(ArtifactPath::parse("solo").unwrap().stem(), "solo");
        assert_eq!(ArtifactPath::parse("a/b/c").unwrap().stem(), "a");
    }
}
