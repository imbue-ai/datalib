//! The vocabulary a provider's metric table is written in: a quantity
//! is one plot, a metric is one column or tag of the raw store and
//! says which plot it draws on, on which axis, in what unit.

/// Which group a metric charts with within its quantity: the metrics on
/// the right get a chart of their own, so a running total does not
/// flatten a per-sample reading drawn beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Left,
    Right,
}

/// A physical quantity: one section of the page, its charts, N series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantity {
    /// Stable slug: the markdown section anchor. Never derived from a
    /// display string.
    pub key: &'static str,
    /// The section heading.
    pub title: &'static str,
    /// One-line explanation rendered under the section heading.
    pub blurb: &'static str,
}

/// How one stored metric maps onto a plot.
#[derive(Debug, Clone, Copy)]
pub struct MetricSpec {
    /// The provider's metric name, as its raw store spells it.
    pub metric: &'static str,
    /// Which section of the page this metric's series belong in.
    pub quantity: Quantity,
    pub axis: Axis,
    /// Appended to the device name in the legend, e.g. `water_valve
    /// (consumption)`. `None` when the quantity has only one metric and
    /// the device name alone is unambiguous.
    pub series_suffix: Option<&'static str>,
    /// The unit the page's device tables show a value in, after
    /// [`MetricSpec::to_si`].
    pub si_unit: &'static str,
    /// The unit the raw store holds the value in, as a UCUM code: what
    /// its time series is stored in, unconverted.
    pub unit: &'static str,
    /// Stored value → the value the device tables show.
    pub to_si: fn(f64) -> f64,
}

pub fn identity(v: f64) -> f64 {
    v
}

pub fn spec_in<'a>(table: &'a [MetricSpec], metric: &str) -> Option<&'a MetricSpec> {
    table.iter().find(|m| m.metric == metric)
}

pub fn series_label(device: &str, spec: &MetricSpec) -> String {
    match spec.series_suffix {
        Some(s) => format!("{device} ({s})"),
        None => device.to_string(),
    }
}

/// The checks every provider's metric table owes: no metric twice, a
/// unit that is UCUM, and no metric pointing at a quantity its
/// `QUANTITIES` does not list. Call from a test.
pub fn check_table(table: &[MetricSpec], quantities: &[Quantity]) {
    let mut seen = std::collections::HashSet::new();
    for m in table {
        assert!(seen.insert(m.metric), "duplicate metric row {}", m.metric);
        if let Err(e) = datalib_time_series_schema::Unit::parse(m.unit) {
            panic!("{}: {e}", m.metric);
        }
        assert!(
            quantities.contains(&m.quantity),
            "{} points at a quantity missing from QUANTITIES",
            m.metric
        );
    }
    let mut keys = std::collections::HashSet::new();
    for q in quantities {
        assert!(keys.insert(q.key), "duplicate quantity key {}", q.key);
    }
}
