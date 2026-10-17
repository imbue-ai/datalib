//! A sensor source's readings as time series: one per device and
//! metric, in the unit the raw store holds it, about the device's row on
//! the source's page. YoLink and AirVisual share this; each hands over
//! its parse and its metric table.

use anyhow::{Context, Result};
use datalib_id::composite_key;
use datalib_time_series_schema::{Points, TimeKind, TimeSeries};

use crate::page::PageProfile;
use crate::series::Series;
use crate::units::spec_in;

const KIND_TIME_SERIES: &str = "time_series";

/// A device as its series name it (`key`) and as a list shows it.
pub struct DeviceName<'a> {
    pub key: &'a str,
    pub name: &'a str,
}

pub fn time_series_id(
    profile: &PageProfile,
    source_id: &str,
    device: &str,
    metric: &str,
) -> String {
    datalib_id::entity_id_str(
        profile.id_namespace,
        source_id,
        None,
        KIND_TIME_SERIES,
        &composite_key(&[device, metric]),
        None,
    )
}

/// Where a document wants a chart of `ids`: the document view draws one
/// there from the time-series store (`docs/dev/time_series.md`
/// §"Charts in documents"). An id is minted hex and dashes; anything
/// else is left out rather than written into an attribute.
pub fn time_series_chart(ids: &[String]) -> String {
    let ids: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .collect();
    format!(
        "<div class=\"time-series-chart\" data-time-series=\"{}\"></div>",
        ids.join(" ")
    )
}

/// One time series per series in `series`. A metric the profile's table
/// does not name is an error, as it is for the page.
pub fn time_series(
    profile: &PageProfile,
    source_id: &str,
    devices: &[DeviceName<'_>],
    series: &[Series],
) -> Result<Vec<TimeSeries>> {
    series
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| {
            let spec = spec_in(profile.metrics, &s.metric).with_context(|| {
                format!(
                    "{} metric {:?} has no row in its metric table",
                    profile.source_label, s.metric
                )
            })?;
            let device = devices
                .iter()
                .find(|d| d.key == s.device)
                .map_or(s.device.as_str(), |d| d.name);
            let human_name = match spec.series_suffix {
                Some(suffix) => format!("{} ({suffix})", spec.quantity.title),
                None => spec.quantity.title.to_string(),
            };
            Ok(TimeSeries {
                time_series_id: time_series_id(profile, source_id, &s.device, &s.metric),
                source_id: source_id.to_string(),
                about_uuid: Some(profile.device_uuid(source_id, &s.device)),
                about_name: Some(device.to_string()),
                name: format!("{}/{}", s.device, s.metric),
                human_name,
                unit: spec.unit.to_string(),
                time_kind: TimeKind::Instant,
                points: Points::floats(s.ts_ms.iter().copied().zip(s.values.iter().copied())),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chart_names_its_ids_and_nothing_that_could_leave_the_attribute() {
        let ids = vec![
            "0190-ab".to_string(),
            "x\" onload=\"y".to_string(),
            "c3".into(),
        ];
        assert_eq!(
            time_series_chart(&ids),
            "<div class=\"time-series-chart\" data-time-series=\"0190-ab c3\"></div>"
        );
    }
}
