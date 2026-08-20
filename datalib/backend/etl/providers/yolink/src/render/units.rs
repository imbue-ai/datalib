//! The one place YoLink's per-metric unit policy lives: which physical
//! quantity a metric belongs to, which axis it draws on, and how to get
//! from the unit the downloader stored to the SI unit we plot.
//!
//! ## Why a table rather than per-call-site `if metric == …`
//!
//! `yolink_readings.metric` carries its unit in the tag
//! (`temperature_c`, `water_meter_gal`) — the value column is a bare
//! `REAL`. That makes "which unit is this?" a question only a lookup
//! can answer, and answering it in more than one place is how a plot
//! ends up with gallons and litres stacked on one axis. Every consumer
//! goes through [`spec_for`].
//!
//! ## What today's downloader can actually emit
//!
//! Only four rows below are reachable from the current pipeline:
//! `temperature_c`, `humidity_pct`, `water_meter_gal`, and
//! `water_consumption_gal`. `download/mod.rs` pins each device kind to a
//! fixed CSV header *and* checks every value's unit suffix, so a `℉`
//! reading under a `℃` header is rejected at parse time rather than
//! silently converted — there is no path that writes a `temperature_f`
//! row today.
//!
//! The `_f` / `_l` rows are here anyway, and that is deliberate: they
//! are the conversion *policy* for the day a device does report in
//! imperial or metric-volume units, sitting next to the units they
//! convert to, rather than a decision deferred to whoever hits the
//! problem. They are covered by the unit tests at the bottom of this
//! file; they are NOT covered by any end-to-end fixture, because no
//! fixture can produce them.

/// Which y-axis a metric draws on within its quantity's plot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// The plot's primary (left-hand) y-axis, Plotly's `y`.
    Left,
    /// A secondary (right-hand) overlaying y-axis, Plotly's `y2`.
    Right,
}

/// A physical quantity — one scatter plot, one HTML file, N device
/// series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantity {
    /// Stable slug: the plot's filename stem (`plots/<key>.html`) and
    /// the markdown section anchor. Never derived from a display
    /// string, so retitling a plot doesn't orphan its file.
    pub key: &'static str,
    /// Section heading + plot title.
    pub title: &'static str,
    /// SI unit label on the primary y-axis.
    pub left_unit: &'static str,
    /// SI unit label on the secondary y-axis, when any metric in this
    /// quantity draws on [`Axis::Right`]. `None` → single-axis plot.
    pub right_unit: Option<&'static str>,
    /// One-line explanation rendered under the section heading.
    pub blurb: &'static str,
}

pub const TEMPERATURE: Quantity = Quantity {
    key: "temperature",
    title: "Temperature",
    left_unit: "°C",
    right_unit: None,
    blurb: "Every temperature-capable device, in degrees Celsius.",
};

pub const HUMIDITY: Quantity = Quantity {
    key: "humidity",
    title: "Relative humidity",
    left_unit: "%RH",
    right_unit: None,
    blurb: "Relative humidity is already a dimensionless ratio — no conversion applies.",
};

/// Liquid volume. Both water metrics are litres, but they are not the
/// same *kind* of number: `water_meter_*` is a lifetime totalizer that
/// only ever climbs (tens of thousands of litres), while
/// `water_consumption_*` is the volume used since the previous sample
/// (hundreds at most). Sharing one axis would flatten consumption into
/// a line along the bottom, so the totalizer overlays on a right-hand
/// axis and both stay readable in one frame.
pub const VOLUME: Quantity = Quantity {
    key: "volume",
    title: "Liquid volume",
    left_unit: "L (per sample)",
    right_unit: Some("L (cumulative)"),
    blurb: "Per-sample consumption on the left axis; the meter's lifetime \
            total on the right axis. Click a legend entry to isolate a series.",
};

/// Every quantity, in the order their sections appear in the document.
pub const QUANTITIES: &[Quantity] = &[TEMPERATURE, HUMIDITY, VOLUME];

/// How one `yolink_readings.metric` value maps onto a plot.
#[derive(Debug, Clone, Copy)]
pub struct MetricSpec {
    /// The literal `yolink_readings.metric` string.
    pub metric: &'static str,
    /// Which plot this metric's series belong on.
    pub quantity: Quantity,
    pub axis: Axis,
    /// Appended to the device name in the legend, e.g. `water_valve
    /// (consumption)`. `None` when the quantity has only one metric and
    /// the device name alone is unambiguous.
    pub series_suffix: Option<&'static str>,
    /// Unit the plotted (converted) value is in. Distinct from
    /// [`Quantity::left_unit`], which is an axis label.
    pub si_unit: &'static str,
    /// Stored value → SI value.
    pub to_si: fn(f64) -> f64,
}

/// US liquid gallon, exactly. The YoLink CSV header says `GAL`; YoLink
/// is a US-market product line, so that is the US liquid gallon
/// (231 in³), not the imperial one — a 20% difference, so it is worth
/// being explicit about which.
pub const US_GALLON_LITRES: f64 = 3.785_411_784;

fn identity(v: f64) -> f64 {
    v
}
fn gallons_to_litres(v: f64) -> f64 {
    v * US_GALLON_LITRES
}
fn fahrenheit_to_celsius(v: f64) -> f64 {
    (v - 32.0) * 5.0 / 9.0
}

/// The complete metric → plot mapping. Adding a metric to the
/// downloader means adding a row here; [`spec_for`] returning `None` is
/// a hard render error rather than a silently dropped series.
pub const METRICS: &[MetricSpec] = &[
    MetricSpec {
        metric: "temperature_c",
        quantity: TEMPERATURE,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "°C",
        to_si: identity,
    },
    MetricSpec {
        metric: "temperature_f",
        quantity: TEMPERATURE,
        axis: Axis::Left,
        series_suffix: None,
        si_unit: "°C",
        to_si: fahrenheit_to_celsius,
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
        metric: "water_consumption_gal",
        quantity: VOLUME,
        axis: Axis::Left,
        series_suffix: Some("consumption"),
        si_unit: "L",
        to_si: gallons_to_litres,
    },
    MetricSpec {
        metric: "water_consumption_l",
        quantity: VOLUME,
        axis: Axis::Left,
        series_suffix: Some("consumption"),
        si_unit: "L",
        to_si: identity,
    },
    MetricSpec {
        metric: "water_meter_gal",
        quantity: VOLUME,
        axis: Axis::Right,
        series_suffix: Some("meter total"),
        si_unit: "L",
        to_si: gallons_to_litres,
    },
    MetricSpec {
        metric: "water_meter_l",
        quantity: VOLUME,
        axis: Axis::Right,
        series_suffix: Some("meter total"),
        si_unit: "L",
        to_si: identity,
    },
];

/// Look up a metric's plot mapping. `None` means the metric is not in
/// [`METRICS`] — callers should fail loudly (see the module docs for
/// why a silent drop is the wrong response).
pub fn spec_for(metric: &str) -> Option<&'static MetricSpec> {
    METRICS.iter().find(|m| m.metric == metric)
}

/// Legend label for one (device, metric) series.
pub fn series_label(device: &str, spec: &MetricSpec) -> String {
    match spec.series_suffix {
        Some(s) => format!("{device} ({s})"),
        None => device.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_is_unique_and_agrees_with_its_quantity() {
        let mut seen = std::collections::HashSet::new();
        for m in METRICS {
            assert!(seen.insert(m.metric), "duplicate metric row {}", m.metric);
            assert!(
                QUANTITIES.contains(&m.quantity),
                "{} points at a quantity missing from QUANTITIES",
                m.metric
            );
            if m.axis == Axis::Right {
                assert!(
                    m.quantity.right_unit.is_some(),
                    "{} draws on y2 but {} declares no right_unit",
                    m.metric,
                    m.quantity.key
                );
            }
        }
    }

    #[test]
    fn quantity_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for q in QUANTITIES {
            assert!(seen.insert(q.key), "duplicate quantity key {}", q.key);
        }
    }

    #[test]
    fn gallons_convert_to_litres() {
        let s = spec_for("water_meter_gal").unwrap();
        // 1 US gallon is 3.785411784 L exactly.
        assert!(((s.to_si)(1.0) - 3.785_411_784).abs() < 1e-12);
        assert!(((s.to_si)(0.0)).abs() < 1e-12);
        // The value that motivated the whole exercise: the live store's
        // max meter reading, 52704.385 gal.
        assert!(((s.to_si)(52_704.385) - 199_507.800_047_47).abs() < 1e-6);
    }

    #[test]
    fn fahrenheit_converts_to_celsius() {
        let s = spec_for("temperature_f").unwrap();
        assert!(((s.to_si)(32.0)).abs() < 1e-12);
        assert!(((s.to_si)(212.0) - 100.0).abs() < 1e-12);
        assert!(((s.to_si)(-40.0) + 40.0).abs() < 1e-12);
    }

    #[test]
    fn celsius_and_fahrenheit_share_one_axis() {
        // The point of the table: two devices reporting different units
        // land on the same plot, in the same unit.
        let c = spec_for("temperature_c").unwrap();
        let f = spec_for("temperature_f").unwrap();
        assert_eq!(c.quantity.key, f.quantity.key);
        assert_eq!(c.si_unit, f.si_unit);
        // -18.4℃ and the same temperature in ℉ must plot identically.
        assert!(((c.to_si)(-18.4) - (f.to_si)(-1.12)).abs() < 1e-9);
    }

    #[test]
    fn water_metrics_split_across_axes() {
        assert_eq!(spec_for("water_consumption_gal").unwrap().axis, Axis::Left);
        assert_eq!(spec_for("water_meter_gal").unwrap().axis, Axis::Right);
    }

    #[test]
    fn unknown_metric_has_no_spec() {
        assert!(spec_for("pressure_psi").is_none());
    }

    #[test]
    fn labels_disambiguate_only_where_needed() {
        assert_eq!(
            series_label("water_valve", spec_for("water_meter_gal").unwrap()),
            "water_valve (meter total)"
        );
        assert_eq!(
            series_label("main_fridge", spec_for("temperature_c").unwrap()),
            "main_fridge"
        );
    }
}
