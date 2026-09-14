//! Provider-owned config schema for the `airvisual` source: IQAir's
//! AirVisual monitors. Schema-only (serde + anyhow), so anything that
//! needs to understand the config can link it without the ingest code.
//! The one method is `export`: each device's own data folder, read off
//! its Samba share or from a copy.

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirvisualExport {
    /// One entry per Pro. Each has its own share, so each has its own
    /// path.
    #[serde(default)]
    pub devices: Vec<AirvisualDevice>,
}

/// One AirVisual Pro's data folder: `smb://<ip>/airvisual` mounted, or
/// the `*_AirVisual_values.txt` files copied out of it, archives
/// included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirvisualDevice {
    pub path: PathBuf,
    /// The device's identity — its serial number, which keys every
    /// sample. Read from the folder's `latest_config_measurements.json`
    /// when left out; required for a copy that lacks the file.
    #[serde(default)]
    pub serial: Option<String>,
    /// What to call it. Defaults to the name the device gives itself
    /// (`node_name` in the same file), then to the serial.
    #[serde(default)]
    pub name: Option<String>,
}

impl AirvisualDevice {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

impl AirvisualConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let Some(export) = &self.export else {
            anyhow::bail!(
                "airvisual: an ingest step needs `[steps.params.export]` with at least one \
                 `[[steps.params.export.devices]]`"
            );
        };
        if export.devices.is_empty() {
            anyhow::bail!("airvisual: export.devices must list at least one device");
        }
        let mut serials: Vec<&str> = Vec::new();
        for (i, d) in export.devices.iter().enumerate() {
            if d.path.as_os_str().is_empty() {
                anyhow::bail!("airvisual: export.devices[{i}].path is empty");
            }
            if let Some(serial) = &d.serial {
                if serial.trim().is_empty() || serial.contains('#') || serial.contains('/') {
                    anyhow::bail!(
                        "airvisual: export.devices[{i}].serial {serial:?} must be non-empty and \
                         contain neither '#' nor '/' (it keys every sample id)"
                    );
                }
                if serials.contains(&serial.as_str()) {
                    anyhow::bail!("airvisual: serial {serial:?} is listed twice");
                }
                serials.push(serial);
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

    fn dev(path: &str, serial: Option<&str>) -> AirvisualDevice {
        AirvisualDevice {
            path: PathBuf::from(path),
            serial: serial.map(str::to_string),
            name: None,
        }
    }

    fn cfg(devices: Vec<AirvisualDevice>) -> AirvisualConfig {
        AirvisualConfig {
            common: Default::default(),
            export: Some(AirvisualExport { devices }),
        }
    }

    #[test]
    fn no_export_and_no_devices_are_errors() {
        assert!(AirvisualConfig::default().validate().is_err());
        assert!(cfg(vec![]).validate().is_err());
    }

    #[test]
    fn a_path_alone_is_enough() {
        assert!(cfg(vec![dev("/Volumes/airvisual", None)])
            .validate()
            .is_ok());
    }

    #[test]
    fn serials_are_id_safe_and_unique() {
        assert!(cfg(vec![dev("/a", Some("4133WV2JB9Z"))]).validate().is_ok());
        assert!(cfg(vec![dev("/a", Some("a#b"))]).validate().is_err());
        assert!(cfg(vec![dev("/a", Some("a/b"))]).validate().is_err());
        assert!(cfg(vec![dev("/a", Some("  "))]).validate().is_err());
        assert!(cfg(vec![dev("/a", Some("X")), dev("/b", Some("X"))])
            .validate()
            .is_err());
    }

    #[test]
    fn an_empty_path_is_an_error() {
        assert!(cfg(vec![dev("", None)]).validate().is_err());
    }
}
