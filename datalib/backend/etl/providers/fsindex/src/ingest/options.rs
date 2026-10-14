//! The per-directory options file, `.fsindex.toml`: parsing, cascading,
//! and breadcrumb writes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const OPTIONS_FILENAME: &str = ".fsindex.toml";

/// Verbatim contents of one `.fsindex.toml` file. Round-trips
/// user-edited fields so writing a breadcrumb (`identity:`) does not
/// drop a user's `ignore:` list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsindexOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp_me_with_uuid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
}

/// Machine-managed breadcrumb block. See EXTRACT.md §"Stamping policy."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub uuid: String,
    pub stamped_at: String,
    pub stamper_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originally_at: Option<String>,
}

/// Effective options at some subtree depth — what the walker actually
/// applies. Built by [`OptionsCascade::effective`] from the root→leaf
/// stack.
#[derive(Debug, Clone, Default)]
pub struct EffectiveOptions {
    pub ignore_patterns: Vec<String>,
    pub stamp_me_with_uuid: bool,
}

/// A root→leaf stack of (path, parsed options) frames. Push on descent,
/// pop on ascent. The cascade rule for `ignore` is *accumulation*
/// (children add patterns); for `stamp_me_with_uuid` it is *override*
/// (deepest non-None wins).
#[derive(Debug, Default, Clone)]
pub struct OptionsCascade {
    frames: Vec<(PathBuf, FsindexOptions)>,
}

impl OptionsCascade {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, dir: PathBuf, options: FsindexOptions) {
        self.frames.push((dir, options));
    }

    pub fn pop(&mut self) {
        self.frames.pop();
    }

    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    pub fn frame_for(&self, dir: &Path) -> Option<&FsindexOptions> {
        self.frames
            .iter()
            .rev()
            .find_map(|(p, y)| if p == dir { Some(y) } else { None })
    }

    pub fn effective(&self) -> EffectiveOptions {
        let mut ignore: Vec<String> = Vec::new();
        let mut stamp = false;
        for (_, y) in &self.frames {
            ignore.extend(y.ignore.iter().cloned());
            if let Some(v) = y.stamp_me_with_uuid {
                stamp = v;
            }
        }
        EffectiveOptions {
            ignore_patterns: ignore,
            stamp_me_with_uuid: stamp,
        }
    }
}

pub fn load_at(dir: &Path) -> Result<Option<FsindexOptions>> {
    let path = dir.join(OPTIONS_FILENAME);
    let text = match fs::read_to_string(&path) {
        Ok(b) => b,
        // `NotFound` is the common "this dir has no breadcrumb" case.
        // `NotADirectory` means the caller passed a non-directory path
        // (file, socket, …) whose join with `.fsindex.toml` is a
        // path that can't exist by definition. The cascade builder
        // overshoots ancestor walks by one frame for files, so this
        // is expected and silent — see [`ancestor_chain`] in walker.rs.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| format!("read {}", path.display()))
        }
    };
    let parsed: FsindexOptions =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(parsed))
}

/// In one rename, so a scan never reads half a breadcrumb.
pub fn write_breadcrumb(dir: &Path, options: &FsindexOptions) -> Result<()> {
    let path = dir.join(OPTIONS_FILENAME);
    let text = toml::to_string(options)
        .with_context(|| format!("serialize breadcrumb for {}", dir.display()))?;
    datalib_runtime::atomic::write(&path, text.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

/// Blake3-hex over a stable canonical encoding of the effective
/// options. *Only* options content — no scanner_version, no platform.
/// Two scans on the same tree with the same ignore set + stamp flag
/// produce the same fingerprint regardless of binary version.
pub fn options_fingerprint(effective: &EffectiveOptions) -> String {
    let dedup: BTreeSet<&String> = effective.ignore_patterns.iter().collect();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"ignore:\n");
    for p in &dedup {
        buf.extend_from_slice(p.as_bytes());
        buf.push(b'\n');
    }
    buf.extend_from_slice(b"stamp_me_with_uuid:");
    buf.push(if effective.stamp_me_with_uuid {
        b'1'
    } else {
        b'0'
    });
    buf.push(b'\n');
    let h = blake3::hash(&buf);
    h.to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A breadcrumb the scanner writes reads back with every field, and
    /// keeps the person's own keys beside the identity block.
    #[test]
    fn a_written_breadcrumb_reads_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(OPTIONS_FILENAME),
            "ignore = [\"*.tmp\", \"holodeck/**\"]\nstamp_me_with_uuid = true\n",
        )
        .unwrap();
        let mut options = load_at(dir.path()).unwrap().expect("an options file");
        assert_eq!(options.ignore, ["*.tmp", "holodeck/**"]);
        assert_eq!(options.stamp_me_with_uuid, Some(true));
        assert!(options.identity.is_none());

        options.identity = Some(Identity {
            uuid: "00000000-0000-8009-9b3f-148e4244d924".into(),
            stamped_at: "2364-10-02T15:06:29+00:00".into(),
            stamper_version: 1,
            originally_at: Some("decks/ten".into()),
        });
        write_breadcrumb(dir.path(), &options).unwrap();

        let reread = load_at(dir.path()).unwrap().expect("the breadcrumb");
        assert_eq!(reread.ignore, options.ignore);
        assert_eq!(reread.stamp_me_with_uuid, Some(true));
        let identity = reread.identity.expect("identity");
        assert_eq!(identity.uuid, "00000000-0000-8009-9b3f-148e4244d924");
        assert_eq!(identity.stamped_at, "2364-10-02T15:06:29+00:00");
        assert_eq!(identity.stamper_version, 1);
        assert_eq!(identity.originally_at.as_deref(), Some("decks/ten"));
    }
}
