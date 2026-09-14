//! Provider-owned config schema for the `airvisual` source: IQAir's
//! AirVisual monitors. Schema-only (serde + anyhow), so anything that
//! needs to understand the config can link it without the ingest code.
//! Ingest-only today: the one method is `export`, the AirVisual Pro's
//! own data folder, read off its Samba share or from a copy.

use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirvisualConfig {
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub export: Option<AirvisualExport>,
}

/// The Pro's data folder: `smb://<ip>/airvisual` mounted, or the
/// `*_AirVisual_values.txt` files copied out of it, archives included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirvisualExport {
    pub path: PathBuf,
    /// The row key for this device's readings; renaming it re-keys the
    /// history. Defaults to the `node_name` in the folder's
    /// `latest_config_measurements.json`, which a copy may not carry.
    #[serde(default)]
    pub device: Option<String>,
}

impl AirvisualExport {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

impl AirvisualConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let Some(export) = &self.export else {
            anyhow::bail!("airvisual: an ingest step needs `[steps.params.export]` with a `path`");
        };
        if export.path.as_os_str().is_empty() {
            anyhow::bail!("airvisual: export.path is empty");
        }
        if let Some(device) = &export.device {
            if device.trim().is_empty() {
                anyhow::bail!(
                    "airvisual: export.device is empty; omit it to read the device's own name"
                );
            }
            if device.contains('#') || device.contains('/') {
                anyhow::bail!(
                    "airvisual: export.device {device:?} may not contain '#' or '/' (it keys every reading id)"
                );
            }
        }
        Ok(())
    }
}

pub type AirvisualRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for AirvisualConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("export")];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(path: &str, device: Option<&str>) -> AirvisualConfig {
        AirvisualConfig {
            common: Default::default(),
            export: Some(AirvisualExport {
                path: PathBuf::from(path),
                device: device.map(str::to_string),
            }),
        }
    }

    #[test]
    fn no_export_is_an_error() {
        assert!(AirvisualConfig::default().validate().is_err());
    }

    #[test]
    fn path_alone_is_enough() {
        assert!(cfg("/Volumes/airvisual", None).validate().is_ok());
    }

    #[test]
    fn device_name_is_id_safe() {
        assert!(cfg("/Volumes/airvisual", Some("Cucina")).validate().is_ok());
        assert!(cfg("/Volumes/airvisual", Some("a#b")).validate().is_err());
        assert!(cfg("/Volumes/airvisual", Some("a/b")).validate().is_err());
        assert!(cfg("/Volumes/airvisual", Some("  ")).validate().is_err());
    }

    #[test]
    fn empty_path_is_an_error() {
        assert!(cfg("", None).validate().is_err());
    }
}
