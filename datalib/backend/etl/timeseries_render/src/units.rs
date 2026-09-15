//! The vocabulary a provider's metric table is written in: a quantity
//! is one plot, a metric is one column or tag of the raw store and
//! says which plot it draws on, on which axis, in what unit.

/// Which y-axis a metric draws on within its quantity's plot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// The plot's primary (left-hand) y-axis, Plotly's `y`.
    Left,
    /// A secondary (right-hand) overlaying y-axis, Plotly's `y2`.
    Right,
}

/// A physical quantity — one scatter plot, one HTML file, N series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantity {
    /// Stable slug: the plot's filename stem (`plots/<key>.html`) and
    /// the markdown section anchor. Never derived from a display
    /// string, so retitling a plot doesn't orphan its file.
    pub key: &'static str,
    /// Section heading + plot title.
    pub title: &'static str,
    /// Unit label on the primary y-axis.
    pub left_unit: &'static str,
    /// Unit label on the secondary y-axis, when any metric in this
    /// quantity draws on [`Axis::Right`]. `None` → single-axis plot.
    pub right_unit: Option<&'static str>,
    /// One-line explanation rendered under the section heading.
    pub blurb: &'static str,
}

/// How one stored metric maps onto a plot.
#[derive(Debug, Clone, Copy)]
pub struct MetricSpec {
    /// The provider's metric name, as its raw store spells it.
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
    /// Stored value → plotted value.
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

/// The two checks every provider's metric table owes: no metric twice,
/// and no metric pointing at a quantity its `QUANTITIES` does not list
/// or at a right axis its quantity does not declare. Call from a test.
pub fn check_table(table: &[MetricSpec], quantities: &[Quantity]) {
    let mut seen = std::collections::HashSet::new();
    for m in table {
        assert!(seen.insert(m.metric), "duplicate metric row {}", m.metric);
        assert!(
            quantities.contains(&m.quantity),
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
    let mut keys = std::collections::HashSet::new();
    for q in quantities {
        assert!(keys.insert(q.key), "duplicate quantity key {}", q.key);
    }
}
