//! Yolink's readings as time series, one per device and metric
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
            key: &d.name,
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
            "yolink",
            &[DeviceName {
                key: "main_fridge",
                name: "main_fridge",
            }],
            &[Series {
                device: "main_fridge".into(),
                metric: "temperature_f".into(),
                ts_ms: vec![1_000, 2_000],
                values: vec![37.4, 37.6],
            }],
        )
        .unwrap();
        let s = &all[0];
        assert_eq!(s.unit, "[degF]", "stored as the device reported it");
        assert_eq!(s.about_name.as_deref(), Some("main_fridge"));
        assert_eq!(
            s.about_uuid.as_deref(),
            Some(render::device_uuid("yolink", "main_fridge").as_str())
        );
        assert_eq!(s.human_name, "Temperature");
        assert_eq!(s.name, "main_fridge/temperature_f");
        let mut unknown = Series {
            device: "main_fridge".into(),
            metric: "temperature_f".into(),
            ts_ms: vec![1_000, 2_000],
            values: vec![37.4, 37.6],
        };
        unknown.metric = "pressure_psi".into();
        assert!(time_series(
            &PROFILE,
            "yolink",
            &[DeviceName {
                key: "main_fridge",
                name: "main_fridge"
            }],
            &[unknown]
        )
        .is_err());
    }
}
