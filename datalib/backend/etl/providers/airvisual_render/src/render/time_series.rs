//! AirVisual's readings as time series, one per device and metric
//! (`datalib_etl_timeseries_render::time_series`).

use std::path::Path;

use anyhow::Result;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_timeseries_render::time_series::{time_series, DeviceName};
use datalib_time_series_schema::TimeSeries;

use super::parse::parse;
use super::render::PROFILE;

/// The raw store's time series at its head.
pub fn read(raw_path: &Path, source_id: &str) -> Result<Vec<TimeSeries>> {
    let parsed = parse(
        raw_path,
        RawRange {
            cursor: None,
            pin: None,
            stale: None,
        },
    )?;
    let devices: Vec<DeviceName<'_>> = parsed
        .devices
        .iter()
        .map(|d| DeviceName {
            key: &d.id,
            name: &d.name,
        })
        .collect();
    time_series(&PROFILE, source_id, &devices, &parsed.series)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::render;
    use datalib_etl_timeseries_render::series::Series;

    #[test]
    fn a_device_metric_is_one_time_series_about_the_devices_row() {
        let all = time_series(
            &PROFILE,
            "airvisual",
            &[DeviceName {
                key: "SN123",
                name: "Bridge",
            }],
            &[Series {
                device: "SN123".into(),
                metric: "pm25_ugm3".into(),
                ts_ms: vec![1_000, 2_000],
                values: vec![4.0, 5.5],
            }],
        )
        .unwrap();
        let s = &all[0];
        assert_eq!(s.unit, "ug/m3");
        assert_eq!(
            s.about_name.as_deref(),
            Some("Bridge"),
            "named, though keyed by serial"
        );
        assert_eq!(
            s.about_uuid.as_deref(),
            Some(render::device_uuid("airvisual", "SN123").as_str())
        );
        assert_eq!(s.name, "SN123/pm25_ugm3");
        let mut unknown = Series {
            device: "SN123".into(),
            metric: "pm25_ugm3".into(),
            ts_ms: vec![1_000, 2_000],
            values: vec![4.0, 5.5],
        };
        unknown.metric = "radon_bq".into();
        assert!(time_series(
            &PROFILE,
            "airvisual",
            &[DeviceName {
                key: "SN123",
                name: "Bridge"
            }],
            &[unknown]
        )
        .is_err());
    }
}
