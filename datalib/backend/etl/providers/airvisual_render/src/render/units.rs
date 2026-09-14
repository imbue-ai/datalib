//! The one place the per-column plot policy lives: which physical
//! quantity each `airvisual_samples` measurement column belongs to,
//! which axis it draws on, and its unit. The Pro already reports in
//! SI, so every conversion here is the identity; the field exists so
//! this table has the same shape as yolink's.

use datalib_etl_timeseries_render::units::identity;
pub use datalib_etl_timeseries_render::units::{series_label, Axis, MetricSpec, Quantity};

pub const PARTICULATES: Quantity = Quantity {
    key: "particulates",
    title: "Particulate matter",
    left_unit: "µg/m³",
    right_unit: None,
    blurb: "PM1, PM2.5 and PM10 mass concentration from the device's laser \
            particle counter. PM1 ⊂ PM2.5 ⊂ PM10, so the three lines nest.",
};

pub const AQI: Quantity = Quantity {
    key: "aqi",
    title: "Air quality index",
    left_unit: "AQI",
    right_unit: None,
    blurb: "The device's own PM2.5 index on the US and Chinese scales, and \
            the index of the outdoor station it follows. An index, not a \
            concentration: 0–50 is good on the US scale.",
};

pub const CO2: Quantity = Quantity {
    key: "co2",
    title: "CO₂",
    left_unit: "ppm",
    right_unit: None,
    blurb: "Carbon dioxide. Outdoor air is ~420 ppm; a closed room with \
            people in it climbs past 1000.",
};

pub const TEMPERATURE: Quantity = Quantity {
    key: "temperature",
    title: "Temperature",
    left_unit: "°C",
    right_unit: None,
    blurb: "Degrees Celsius. The device also logs Fahrenheit; that column \
            is kept in the sample payload and not plotted twice.",
};

pub const HUMIDITY: Quantity = Quantity {
    key: "humidity",
    title: "Relative humidity",
    left_unit: "%RH",
    right_unit: None,
    blurb: "Relative humidity is already a dimensionless ratio — no conversion applies.",
};

pub const VOC: Quantity = Quantity {
    key: "voc",
    title: "Volatile organic compounds",
    left_unit: "ppb",
    right_unit: None,
    blurb: "Only a Pro with the VOC module reports this; the others log a \
            sentinel the ingest step drops.",
};

/// Every quantity, in the order their sections appear in the document.
pub const QUANTITIES: &[Quantity] = &[PARTICULATES, AQI, CO2, TEMPERATURE, HUMIDITY, VOC];

/// The complete column → plot mapping. Every measurement column of
/// `airvisual_samples` has a row here; [`spec_for`] returning `None` is
/// a hard render error rather than a silently dropped series.
pub const METRICS: &[MetricSpec] = &[
    MetricSpec {
        metric: "pm1_ugm3",
        quantity: PARTICULATES,
        axis: Axis::Left,
        series_suffix: Some("PM1"),
        si_unit: "µg/m³",
        to_si: identity,
    },
    MetricSpec {
        metric: "pm25_ugm3",
        quantity: PARTICULATES,
        axis: Axis::Left,
        series_suffix: Some("PM2.5"),
        si_unit: "µg/m³",
        to_si: identity,
    },
    MetricSpec {
        metric: "pm10_ugm3",
        quantity: PARTICULATES,
        axis: Axis::Left,
        series_suffix: Some("PM10"),
        si_unit: "µg/m³",
        to_si: identity,
    },
    MetricSpec {
        metric: "aqi_us",
        quantity: AQI,
        axis: Axis::Left,
        series_suffix: Some("US"),
        si_unit: "AQI",
        to_si: identity,
    },
    MetricSpec {
        metric: "aqi_cn",
        quantity: AQI,
        axis: Axis::Left,
        series_suffix: Some("CN"),
        si_unit: "AQI",
        to_si: identity,
    },
    MetricSpec {
        metric: "outdoor_aqi_us",
        quantity: AQI,
        axis: Axis::Left,
        series_suffix: Some("outdoor US"),
        si_unit: "AQI",
        to_si: identity,
    },
    MetricSpec {
        metric: "outdoor_aqi_cn",
        quantity: AQI,
        axis: Axis::Left,
        series_suffix: Some("outdoor CN"),
        si_unit: "AQI",
        to_si: identity,
    },
    MetricSpec {
        metric: "co2_ppm",
        quantity: CO2,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "ppm",
        to_si: identity,
    },
    MetricSpec {
        metric: "temperature_c",
        quantity: TEMPERATURE,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "°C",
        to_si: identity,
    },
    MetricSpec {
        metric: "humidity_pct",
        quantity: HUMIDITY,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "%RH",
        to_si: identity,
    },
    MetricSpec {
        metric: "voc_ppb",
        quantity: VOC,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "ppb",
        to_si: identity,
    },
];

/// Look up a column's plot mapping. `None` means the column is not in
/// [`METRICS`] — callers should fail loudly (see the module docs for
/// why a silent drop is the wrong response).
pub fn spec_for(metric: &str) -> Option<&'static MetricSpec> {
    datalib_etl_timeseries_render::units::spec_in(METRICS, metric)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_consistent() {
        datalib_etl_timeseries_render::units::check_table(METRICS, QUANTITIES);
    }

    /// The columns the ingest side writes and the rows this table knows
    /// are the same set, in both directions.
    #[test]
    fn metrics_cover_every_sample_column() {
        use datalib_etl_airvisual::ingest::schema_raw::SAMPLE_MEASUREMENTS;
        for m in METRICS {
            assert!(
                SAMPLE_MEASUREMENTS.contains(&m.metric),
                "{} is not a sample column",
                m.metric
            );
        }
        for col in SAMPLE_MEASUREMENTS {
            assert!(
                spec_for(col).is_some(),
                "sample column {col} has no plot row"
            );
        }
        assert_eq!(METRICS.len(), SAMPLE_MEASUREMENTS.len());
    }

    #[test]
    fn unknown_metric_has_no_spec() {
        assert!(spec_for("pressure_pa").is_none());
    }

    #[test]
    fn labels_disambiguate_only_where_needed() {
        assert_eq!(
            series_label("kitchen", spec_for("pm25_ugm3").unwrap()),
            "kitchen (PM2.5)"
        );
        assert_eq!(
            series_label("kitchen", spec_for("co2_ppm").unwrap()),
            "kitchen"
        );
    }
}
