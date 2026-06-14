//! `.fsindex.yaml` parsing, cascading, and breadcrumb writes.
//!
//! See [`EXTRACT.md`](../../EXTRACT.md) §"Options file" and §"Stamping
//! policy" for what each key means. See
//! [`schema_raw`](super::schema_raw) §"Directory tree-hash
//! canonicalization" for why the breadcrumb is excluded from the
//! directory's blake3 input.
//!
//! The fingerprint produced here lands in
//! `scan_meta.options_fingerprint`. It is intentionally derived only
//! from option *content* (the ignore set + stamping flag), not from
//! platform / scanner state — see EXTRACT.md.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const BREADCRUMB_FILENAME: &str = ".fsindex.yaml";

/// Verbatim contents of one `.fsindex.yaml` file. Round-trips
/// user-edited fields so writing a breadcrumb (`identity:`) does not
/// drop a user's `ignore:` list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsindexYaml {
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

/// A root→leaf stack of (path, parsed yaml) frames. Push on descent,
/// pop on ascent. The cascade rule for `ignore` is *accumulation*
/// (children add patterns); for `stamp_me_with_uuid` it is *override*
/// (deepest non-None wins).
#[derive(Debug, Default, Clone)]
pub struct OptionsCascade {
    frames: Vec<(PathBuf, FsindexYaml)>,
}

impl OptionsCascade {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, dir: PathBuf, yaml: FsindexYaml) {
        self.frames.push((dir, yaml));
    }

    pub fn pop(&mut self) {
        self.frames.pop();
    }

    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The yaml frame matching `dir`, if the cascade currently has one
    /// pushed for that exact path.
    pub fn frame_for(&self, dir: &Path) -> Option<&FsindexYaml> {
        self.frames
            .iter()
            .rev()
            .find_map(|(p, y)| if p == dir { Some(y) } else { None })
    }

    /// Resolve effective options at the deepest frame.
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

/// Load `<dir>/.fsindex.yaml` if it exists. Returns `Ok(None)` when
/// the file is absent.
pub fn load_at(dir: &Path) -> Result<Option<FsindexYaml>> {
    let path = dir.join(BREADCRUMB_FILENAME);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| format!("read {}", path.display()))
        }
    };
    let parsed: FsindexYaml =
        serde_yaml::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(parsed))
}

/// Atomic breadcrumb write. Writes to `<dir>/.fsindex.yaml.tmp` then
/// renames into place, so a partial write never leaves a half-baked
/// breadcrumb the next scan would mis-parse.
///
/// MUST preserve user-edited keys (`ignore`, `stamp_me_with_uuid`)
/// verbatim — callers construct `yaml` by mutating an existing
/// [`load_at`] result.
pub fn write_breadcrumb(dir: &Path, yaml: &FsindexYaml) -> Result<()> {
    let path = dir.join(BREADCRUMB_FILENAME);
    let tmp = dir.join(format!("{BREADCRUMB_FILENAME}.tmp"));
    let text = serde_yaml::to_string(yaml)
        .with_context(|| format!("serialize breadcrumb for {}", dir.display()))?;
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(text.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
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
