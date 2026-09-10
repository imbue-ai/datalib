//! Provider-owned config schema for the `media` source. Schema-only
//! (serde + anyhow), so the orchestrator can name [`MediaConfig`]
//! without linking the provider.

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
    #[serde(default)]
    pub max_bytes: Option<u64>,

    /// Give up on the metadata-excluding payload hash above this size,
    /// leaving `media_items.payload_blake3` NULL.
    #[serde(default = "default_payload_max_bytes")]
    pub payload_max_bytes: Option<u64>,

    /// Index `.m3u` / `.m3u8` playlists found in the tree.
    #[serde(default = "default_true")]
    pub playlists: bool,

    /// Skip files that have no data blocks allocated — cloud
    /// placeholders (Dropbox "online-only", macOS dataless files,
    /// OneDrive stubs) and iCloud's `.icloud` eviction markers.
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

impl datalib_source_common::IngestMethods for MediaConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local(
            "common.input_path",
        )];
}

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
