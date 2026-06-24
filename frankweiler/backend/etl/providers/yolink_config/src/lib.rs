//! Provider-owned config schema for the `yolink` source (Program A goal #1).
//! Schema-only (serde + anyhow), so the orchestrator can name `YolinkConfig`
//! without linking the provider. Yolink is EXTRACT-ONLY: `sync:` present →
//! live per-device CSV mirror; absent → nothing to do (no translate path).
//!
//! These types are copied from `frankweiler_core::config` so this crate stays
//! free of a core dependency. The provider's `processor` converts these into
//! the core `YolinkSync`/`YolinkDevice` its `extract::fetch` still expects.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The yolink-owned slice of a `yolink` source. `sync:` drives the
/// per-device download; absent means no extract is contributed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct YolinkConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<YolinkSync>,
}

/// Per-device download knobs. WARNING: `family_device_id` + `device_udid`
/// are per-device read secrets; anyone with the pair can pull all CSV
/// history for the device, forever. Scrub from any committed/public configs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct YolinkSync {
    /// Re-fetch overlap (minutes). Default 5.
    #[serde(default)]
    pub overlap_minutes: Option<i64>,
    /// Stride (days) between successive window-starts. Default 7.
    #[serde(default)]
    pub window_days: Option<i64>,
    /// Devices to fetch. Each entry's `name` is the row key in the raw DB,
    /// so renaming one re-keys its history — keep it stable.
    #[serde(default)]
    pub devices: Vec<YolinkDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YolinkDevice {
    /// Stable label; becomes the PK in `yolink_devices`. Changing it later
    /// orphans prior history.
    pub name: String,
    /// `temperature_humidity` or `watermeter`.
    pub kind: String,
    /// Earliest timepoint to ever pull, as `YYYY-MM-DD`.
    pub start: String,
    /// First URL path segment — the `<32hex>` in the download URL.
    pub family_device_id: String,
    /// Per-device UUID returned by the YoLink open API as `deviceUDID`.
    /// REDACT before publishing.
    pub device_udid: String,
}

impl YolinkConfig {
    /// Replicates the `SourceConfig::Yolink` rules from
    /// `frankweiler_core::config::Config::validate()`: ≥1 device, unique
    /// device names, `kind ∈ {temperature_humidity, watermeter}`, `start`
    /// is `YYYY-MM-DD`, and `family_device_id`/`device_udid` are 32
    /// lowercase-hex.
    pub fn validate(&self) -> anyhow::Result<()> {
        let Some(sync) = &self.sync else {
            return Ok(());
        };
        if sync.devices.is_empty() {
            anyhow::bail!("yolink: sync.devices must list at least one device");
        }
        let mut dev_names: Vec<&str> = sync.devices.iter().map(|d| d.name.as_str()).collect();
        dev_names.sort_unstable();
        let mut dupes: Vec<String> = dev_names
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0].to_string())
            .collect();
        if !dupes.is_empty() {
            dupes.dedup();
            anyhow::bail!("yolink: duplicate device names: {}", dupes.join(", "));
        }
        for d in &sync.devices {
            match d.kind.as_str() {
                "temperature_humidity" | "watermeter" => {}
                other => anyhow::bail!(
                    "yolink: device {:?} has unknown kind {:?} \
                     (expected temperature_humidity or watermeter)",
                    d.name,
                    other
                ),
            }
            if !is_yyyy_mm_dd(&d.start) {
                anyhow::bail!(
                    "yolink: device {:?} start {:?} is not YYYY-MM-DD",
                    d.name,
                    d.start
                );
            }
            if !is_hex32(&d.family_device_id) {
                anyhow::bail!(
                    "yolink: device {:?} family_device_id {:?} is not 32 lowercase-hex chars",
                    d.name,
                    d.family_device_id
                );
            }
            if !is_hex32(&d.device_udid) {
                anyhow::bail!(
                    "yolink: device {:?} device_udid {:?} is not 32 lowercase-hex chars",
                    d.name,
                    d.device_udid
                );
            }
        }
        Ok(())
    }
}

/// Cheap `YYYY-MM-DD` shape check. Doesn't validate that the date is real
/// (Feb 30 etc.) — the extractor's `NaiveDate::parse_from_str` catches that
/// at runtime. We just want to bounce obvious typos at config-load time.
fn is_yyyy_mm_dd(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> YolinkDevice {
        YolinkDevice {
            name: name.into(),
            kind: "temperature_humidity".into(),
            start: "2026-01-01".into(),
            family_device_id: "0123456789abcdef0123456789abcdef".into(),
            device_udid: "fedcba9876543210fedcba9876543210".into(),
        }
    }

    fn cfg(devices: Vec<YolinkDevice>) -> YolinkConfig {
        YolinkConfig {
            common: Default::default(),
            sync: Some(YolinkSync {
                overlap_minutes: None,
                window_days: None,
                devices,
            }),
        }
    }

    #[test]
    fn no_sync_is_ok() {
        assert!(YolinkConfig::default().validate().is_ok());
    }

    #[test]
    fn valid_device_passes() {
        assert!(cfg(vec![dev("freezer")]).validate().is_ok());
    }

    #[test]
    fn empty_devices_fails() {
        assert!(cfg(vec![]).validate().is_err());
    }

    #[test]
    fn duplicate_names_fail() {
        let err = cfg(vec![dev("a"), dev("a")]).validate().unwrap_err();
        assert!(format!("{err}").contains("duplicate"));
    }

    #[test]
    fn bad_kind_fails() {
        let mut d = dev("x");
        d.kind = "pressure".into();
        assert!(cfg(vec![d]).validate().is_err());
    }

    #[test]
    fn bad_start_fails() {
        let mut d = dev("x");
        d.start = "2026/01/01".into();
        assert!(cfg(vec![d]).validate().is_err());
    }

    #[test]
    fn bad_hex_fails() {
        let mut d = dev("x");
        d.family_device_id = "NOTHEX".into();
        assert!(cfg(vec![d]).validate().is_err());
        let mut d2 = dev("y");
        d2.device_udid = "0123456789ABCDEF0123456789ABCDEF".into(); // uppercase rejected
        assert!(cfg(vec![d2]).validate().is_err());
    }
}
