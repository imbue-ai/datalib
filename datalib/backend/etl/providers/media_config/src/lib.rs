//! Provider-owned config schema for the `media` source. Schema-only
//! (serde + anyhow), so the orchestrator can name [`MediaConfig`]
//! without linking the provider.
//!
//! `media` is purely file-backed: there is no API and no `sync:` block
//! — it scans the tree at `common.input_path` for audio, image, video
//! and playlist files and records what each one is. It is
//! **download-only**; see the provider's `DOWNLOAD.md` §"No render
//! side".

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The media-owned slice of a `media` source. The scan root is
/// `common.input_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`. The scanned tree
    /// is `input_path`.
    #[serde(default)]
    pub common: SourceCommon,

    /// Gitignore-shaped patterns pruned from the scan, in addition to
    /// any `.gitignore` files found in the tree. Matched by the
    /// `ignore` crate (the one ripgrep uses), so `**`, anchored `/`,
    /// and character classes all behave as expected.
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Skip files larger than this entirely — no row at all.
    ///
    /// Defaults to `None`, unlike `pdf`'s 512 MiB ceiling, and the
    /// difference is deliberate. A multi-gigabyte PDF is nearly always
    /// a corrupt file; a multi-gigabyte video is Tuesday. Indexing one
    /// costs a `stat` on every rescan and one streaming hash on the
    /// first, which is exactly what the Unison cursor exists to bound.
    #[serde(default)]
    pub max_bytes: Option<u64>,

    /// Give up on the metadata-excluding payload hash above this size,
    /// leaving `media_items.payload_blake3` NULL.
    ///
    /// Separate from [`Self::max_bytes`] because the two costs are not
    /// the same. The file hash is one sequential read the rescan cursor
    /// then makes free forever; the payload hash is a second pass that
    /// also has to walk a container structure. 8 GiB keeps every
    /// realistic photo and song, and most video, while refusing to let
    /// one 40 GiB master recording dominate a scan. `None` means no
    /// ceiling.
    #[serde(default = "default_payload_max_bytes")]
    pub payload_max_bytes: Option<u64>,

    /// Index `.m3u` / `.m3u8` playlists found in the tree.
    ///
    /// On by default. Turning it off is for trees where the only
    /// playlists are application caches — see the provider's
    /// `DOWNLOAD.md` §"HLS manifests are not playlists" for why
    /// extension alone cannot tell them apart, and what we sniff
    /// instead.
    #[serde(default = "default_true")]
    pub playlists: bool,

    /// Skip files that have no data blocks allocated — cloud
    /// placeholders (Dropbox "online-only", macOS dataless files,
    /// OneDrive stubs) and iCloud's `.icloud` eviction markers.
    ///
    /// On by default, because reading one is not a cheap mistake: it
    /// asks the sync client to materialize the file, so a first scan of
    /// an evicted library would try to pull the whole thing down.
    ///
    /// The detection is `blocks == 0 && size > 0`, which is a
    /// heuristic: a filesystem that reports no block counts at all
    /// would look entirely evicted. That failure is loud rather than
    /// silent — every skip is counted into the step's
    /// `dataless_skipped=` summary and logged — but if you are on such
    /// a filesystem, set this `false`.
    #[serde(default = "default_true")]
    pub skip_dataless: bool,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
            ignore: Vec::new(),
            max_bytes: None,
            payload_max_bytes: default_payload_max_bytes(),
            playlists: true,
            skip_dataless: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 8 GiB. See [`MediaConfig::payload_max_bytes`].
fn default_payload_max_bytes() -> Option<u64> {
    Some(8 * 1024 * 1024 * 1024)
}

impl MediaConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(0) = self.max_bytes {
            anyhow::bail!("media: `max_bytes = 0` would skip every file; omit it for no limit");
        }
        if let Some(0) = self.payload_max_bytes {
            anyhow::bail!(
                "media: `payload_max_bytes = 0` would leave every payload_blake3 NULL; \
                 omit it for no limit, or set `payload_max_bytes` to a real ceiling"
            );
        }
        Ok(())
    }
}

/// Params for the render step. `media` is download-only, so this is the
/// shared bare envelope and the provider's `plan_render` returns no
/// processors — "download-only" is structural (a missing processor),
/// not a flag. Same shape as `fsindex`.
pub type MediaRenderConfig = datalib_source_common::BareRenderConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        MediaConfig::default().validate().unwrap();
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let c: MediaConfig = toml::from_str("").unwrap();
        assert_eq!(c.max_bytes, None, "no indexing ceiling by default");
        assert_eq!(c.payload_max_bytes, Some(8 * 1024 * 1024 * 1024));
        assert!(c.playlists);
        assert!(c.skip_dataless);
    }

    #[test]
    fn zero_ceilings_are_rejected() {
        let c = MediaConfig {
            max_bytes: Some(0),
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = MediaConfig {
            payload_max_bytes: Some(0),
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = toml::from_str::<MediaConfig>("playlistss = true").unwrap_err();
        assert!(e.to_string().contains("playlistss"), "{e}");
    }

    #[test]
    fn opt_outs_round_trip() {
        let c: MediaConfig = toml::from_str("playlists = false\nskip_dataless = false\n").unwrap();
        assert!(!c.playlists);
        assert!(!c.skip_dataless);
    }
}
