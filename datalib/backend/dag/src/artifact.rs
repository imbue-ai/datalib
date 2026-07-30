//! Artifact path patterns and the overlap test that edge derivation is
//! built on.
//!
//! An artifact is addressed by a `/`-separated path relative to
//! `data_root`; the path names the whole tree rooted there (a file is
//! a one-node tree). *Outputs* must be concrete paths — a step has to
//! own an enumerable set of trees so ownership conflicts are
//! checkable. *Inputs* may use wildcards, which is how "the output of
//! all download steps" is expressed without naming each one:
//!
//! * `*`  — exactly one path segment
//! * `**` — zero or more segments
//!
//! Wildcards apply to whole segments only (no `foo*`), which keeps the
//! overlap test decidable by a simple recursion and keeps patterns
//! readable in configs.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Seg {
    Lit(String),
    /// `*` — exactly one segment.
    Star,
    /// `**` — zero or more segments.
    Globstar,
}

/// A parsed artifact path pattern. Concrete iff it contains no
/// wildcard segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactPat {
    segs: Vec<Seg>,
    raw: String,
}

impl ArtifactPat {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let raw = raw.trim_matches('/');
        if raw.is_empty() {
            anyhow::bail!("artifact path must be a non-empty relative path");
        }
        let mut segs = Vec::new();
        for s in raw.split('/') {
            match s {
                "" => anyhow::bail!("artifact path {raw:?} has an empty segment"),
                "." | ".." => anyhow::bail!("artifact path {raw:?} may not contain `.`/`..`"),
                "*" => segs.push(Seg::Star),
                "**" => segs.push(Seg::Globstar),
                lit => {
                    if lit.contains('*') {
                        anyhow::bail!(
                            "artifact path {raw:?}: wildcards must be whole segments (`*`/`**`)"
                        );
                    }
                    segs.push(Seg::Lit(lit.to_string()));
                }
            }
        }
        Ok(Self {
            segs,
            raw: raw.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn is_concrete(&self) -> bool {
        self.segs.iter().all(|s| matches!(s, Seg::Lit(_)))
    }

    /// Does the tree this pattern denotes intersect the tree rooted at
    /// the concrete path `path`? True iff some concrete path exists
    /// that this pattern matches and that is an ancestor-or-descendant
    /// of `path`. This is the edge test: producer output `path`,
    /// consumer input `self`.
    pub fn overlaps(&self, path: &ArtifactPat) -> bool {
        debug_assert!(path.is_concrete());
        let path_segs: Vec<&str> = path
            .segs
            .iter()
            .map(|s| match s {
                Seg::Lit(l) => l.as_str(),
                _ => unreachable!("overlaps() rhs must be concrete"),
            })
            .collect();
        overlap(&self.segs, &path_segs)
    }

    /// Concrete-vs-concrete containment-or-equality in either
    /// direction: do the two trees intersect? Used for output
    /// ownership conflicts.
    pub fn conflicts_with(&self, other: &ArtifactPat) -> bool {
        debug_assert!(self.is_concrete() && other.is_concrete());
        let a: Vec<&str> = self.segs.iter().map(seg_lit).collect();
        let b: Vec<&str> = other.segs.iter().map(seg_lit).collect();
        let n = a.len().min(b.len());
        a[..n] == b[..n]
    }
}

fn seg_lit(s: &Seg) -> &str {
    match s {
        Seg::Lit(l) => l.as_str(),
        _ => panic!("expected concrete artifact path"),
    }
}

/// Pattern-vs-path tree intersection.
///
/// * `pat` exhausted → the pattern matched an ancestor-or-self of
///   `path`; its tree contains `path`. Overlap.
/// * `path` exhausted → the rest of the pattern would have to match
///   *inside* `path`'s tree. That only counts when the remainder is a
///   deterministic dive: trailing `**` (the whole tree — redundant
///   but harmless) or wildcard-free segments (a specific subpath,
///   e.g. input `slack/raw/entities.doltlite_db` against output
///   `slack/raw`). A remainder with wildcards ahead of literals —
///   `**/rendered_md` against output `slack/raw` — is NOT an
///   overlap: under true tree-intersection semantics `**` would
///   overlap every output (something matching could always exist
///   deeper inside), and every wildcard input would depend on every
///   step. Wildcards select among declared output *roots*, they do
///   not search inside them.
fn overlap(pat: &[Seg], path: &[&str]) -> bool {
    let Some((first, rest_pat)) = pat.split_first() else {
        return true;
    };
    if path.is_empty() {
        let rest = if *first == Seg::Globstar {
            rest_pat
        } else {
            pat
        };
        return rest.is_empty()
            || (pat[0] != Seg::Globstar && rest.iter().all(|s| matches!(s, Seg::Lit(_))));
    }
    match first {
        Seg::Globstar => overlap(rest_pat, path) || overlap(pat, &path[1..]),
        Seg::Star => overlap(rest_pat, &path[1..]),
        Seg::Lit(l) => l == path[0] && overlap(rest_pat, &path[1..]),
    }
}

impl fmt::Display for ArtifactPat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for ArtifactPat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for ArtifactPat {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        ArtifactPat::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> ArtifactPat {
        ArtifactPat::parse(s).unwrap()
    }

    #[test]
    fn parse_rejects_bad_paths() {
        assert!(ArtifactPat::parse("").is_err());
        assert!(ArtifactPat::parse("a//b").is_err());
        assert!(ArtifactPat::parse("../x").is_err());
        assert!(ArtifactPat::parse("a/b*.md").is_err());
    }

    #[test]
    fn concrete_exact_and_tree_overlap() {
        // Same path.
        assert!(pat("slack/raw").overlaps(&pat("slack/raw")));
        // Input names a file inside the producer's output tree.
        assert!(pat("slack/raw/entities.doltlite_db").overlaps(&pat("slack/raw")));
        // Input names a tree that contains the producer's output.
        assert!(pat("slack").overlaps(&pat("slack/raw")));
        // Sibling trees don't overlap.
        assert!(!pat("slack/rendered_md").overlaps(&pat("slack/raw")));
        assert!(!pat("email/raw").overlaps(&pat("slack/raw")));
    }

    #[test]
    fn star_matches_exactly_one_segment() {
        assert!(pat("*/raw").overlaps(&pat("slack/raw")));
        assert!(!pat("*/raw").overlaps(&pat("google_takeout/chat/raw")));
        // ...but the matched tree still contains deeper paths.
        assert!(pat("*/raw").overlaps(&pat("slack/raw/entities.doltlite_db")));
    }

    #[test]
    fn globstar_matches_any_depth() {
        assert!(pat("**/rendered_md").overlaps(&pat("slack/rendered_md")));
        assert!(pat("**/rendered_md").overlaps(&pat("google_takeout/chat/rendered_md")));
        assert!(!pat("**/rendered_md").overlaps(&pat("slack/raw")));
        // Globstar in the middle.
        assert!(pat("google_takeout/**/raw").overlaps(&pat("google_takeout/voice/raw")));
    }

    #[test]
    fn trailing_globstar_is_redundant_but_harmless() {
        // A path already names its whole tree; `x/**` behaves the same.
        assert!(pat("slack/raw/**").overlaps(&pat("slack/raw")));
        assert!(pat("slack/raw/**").overlaps(&pat("slack/raw/db")));
    }

    #[test]
    fn conflicts() {
        assert!(pat("slack/raw").conflicts_with(&pat("slack/raw")));
        assert!(pat("slack").conflicts_with(&pat("slack/raw")));
        assert!(pat("slack/raw").conflicts_with(&pat("slack")));
        assert!(!pat("slack/raw").conflicts_with(&pat("slack/rendered_md")));
    }
}
