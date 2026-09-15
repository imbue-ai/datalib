//! One device's values for one metric, ascending by time — what a
//! provider's parse produces and a plot consumes.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Series {
    pub device: String,
    /// The provider's metric name; the key into its metric table.
    pub metric: String,
    /// Unix milliseconds, ascending.
    pub ts_ms: Vec<i64>,
    /// Values as stored, parallel to `ts_ms`. Conversion to the plotted
    /// unit is the metric table's job.
    pub values: Vec<f64>,
}

impl Series {
    pub fn new(device: String, metric: String) -> Self {
        Self {
            device,
            metric,
            ts_ms: Vec::new(),
            values: Vec::new(),
        }
    }

    pub fn push(&mut self, ts_ms: i64, value: f64) {
        self.ts_ms.push(ts_ms);
        self.values.push(value);
    }

    pub fn len(&self) -> usize {
        self.ts_ms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ts_ms.is_empty()
    }
}

pub fn by_device(series: &[Series]) -> BTreeMap<&str, Vec<&Series>> {
    let mut out: BTreeMap<&str, Vec<&Series>> = BTreeMap::new();
    for s in series {
        out.entry(s.device.as_str()).or_default().push(s);
    }
    out
}

pub fn latest_ts_ms(series: &[Series]) -> Option<i64> {
    series.iter().filter_map(|s| s.ts_ms.last()).max().copied()
}

pub fn earliest_ts_ms(series: &[Series]) -> Option<i64> {
    series.iter().filter_map(|s| s.ts_ms.first()).min().copied()
}
