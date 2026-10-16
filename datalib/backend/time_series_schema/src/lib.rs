//! A time series: one named quantity from one source, as values over
//! time. The shape a source's `render_time_series` step writes, the
//! `time_series_index` step indexes and the time-series applet serves.
//! `docs/dev/time_series.md` is the design.

pub mod chunks;
pub mod series;
pub mod sources;
pub mod unit;

use serde::{Deserialize, Serialize};

pub use chunks::TimeSeriesChunkRow;
pub use series::TimeSeriesRow;
pub use sources::TimeSeriesSourceRow;
pub use unit::Unit;

/// What a point's `t` counts.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TimeKind {
    /// A moment: `t` is unix milliseconds.
    Instant,
    /// A calendar day as the person lived it: `t` is days since
    /// 1970-01-01. Never midnight UTC, which is the day before west of
    /// Greenwich.
    Day,
}

impl TimeKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// What every value of one time series is.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ValueType {
    Integer,
    Float,
    /// A reading that is not a number: a sleep stage, a door state.
    Text,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// `t` for a [`TimeKind::Day`] point: days since 1970-01-01 of a
/// `YYYY-MM-DD` date. `None` for anything else.
pub fn day_number(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        d.signed_duration_since(chrono::NaiveDate::default())
            .num_days(),
    )
}

/// The values of a run of points, all of one type.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    Integer(Vec<i64>),
    Float(Vec<f64>),
    Text(Vec<String>),
}

impl Values {
    pub fn value_type(&self) -> ValueType {
        match self {
            Values::Integer(_) => ValueType::Integer,
            Values::Float(_) => ValueType::Float,
            Values::Text(_) => ValueType::Text,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Values::Integer(v) => v.len(),
            Values::Float(v) => v.len(),
            Values::Text(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn empty(value_type: ValueType) -> Self {
        match value_type {
            ValueType::Integer => Values::Integer(Vec::new()),
            ValueType::Float => Values::Float(Vec::new()),
            ValueType::Text => Values::Text(Vec::new()),
        }
    }

    /// The values in `range`, as their own run.
    pub fn slice(&self, range: std::ops::Range<usize>) -> Self {
        match self {
            Values::Integer(v) => Values::Integer(v[range].to_vec()),
            Values::Float(v) => Values::Float(v[range].to_vec()),
            Values::Text(v) => Values::Text(v[range].to_vec()),
        }
    }

    /// The `i`th value as a number; `None` for text.
    pub fn number(&self, i: usize) -> Option<f64> {
        match self {
            Values::Integer(v) => Some(v[i] as f64),
            Values::Float(v) => Some(v[i]),
            Values::Text(_) => None,
        }
    }

    pub fn text(&self, i: usize) -> Option<&str> {
        match self {
            Values::Text(v) => Some(&v[i]),
            _ => None,
        }
    }
}

/// A run of points: `t` strictly ascending, one value per `t`.
#[derive(Debug, Clone, PartialEq)]
pub struct Points {
    pub t: Vec<i64>,
    pub values: Values,
}

impl Points {
    pub fn integers(points: impl IntoIterator<Item = (i64, i64)>) -> Self {
        let (t, v) = points.into_iter().unzip();
        Points {
            t,
            values: Values::Integer(v),
        }
    }

    pub fn floats(points: impl IntoIterator<Item = (i64, f64)>) -> Self {
        let (t, v) = points.into_iter().unzip();
        Points {
            t,
            values: Values::Float(v),
        }
    }

    pub fn texts(points: impl IntoIterator<Item = (i64, String)>) -> Self {
        let (t, v) = points.into_iter().unzip();
        Points {
            t,
            values: Values::Text(v),
        }
    }

    pub fn len(&self) -> usize {
        self.t.len()
    }

    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }

    pub fn slice(&self, range: std::ops::Range<usize>) -> Self {
        Points {
            t: self.t[range.clone()].to_vec(),
            values: self.values.slice(range),
        }
    }

    /// Why these points cannot be stored, if they cannot: a store keys a
    /// point by its `t`, and a chunk is cut by position.
    pub fn problem(&self) -> Option<String> {
        if self.t.len() != self.values.len() {
            return Some(format!(
                "{} times but {} values",
                self.t.len(),
                self.values.len()
            ));
        }
        self.t
            .windows(2)
            .find(|w| w[0] >= w[1])
            .map(|w| format!("t {} is followed by {}, not by a later t", w[0], w[1]))
    }
}

/// A time series as a render builds it, before any store keys it.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSeries {
    /// Minted by the provider from its source id and `name`
    /// (`docs/dev/entity_ids.md`), so it is the same on every render.
    pub time_series_id: String,
    pub source_id: String,
    /// The document this time series belongs to, when it belongs to one
    /// (an activity); `None` for one about the source as a whole.
    pub about_uuid: Option<String>,
    /// The provider's key for what is measured, unique within the
    /// source and `about_uuid`: `resting_hr`.
    pub name: String,
    /// What a chart legend shows: `Resting HR`.
    pub human_name: String,
    /// A UCUM code ([`Unit`]); `1` for a pure number.
    pub unit: String,
    pub time_kind: TimeKind,
    pub points: Points,
}

impl TimeSeries {
    /// The row for this time series under `ts_key`, a key local to the
    /// source's store.
    pub fn row(&self, ts_key: i64) -> TimeSeriesRow {
        TimeSeriesRow {
            source_id: self.source_id.clone(),
            ts_key,
            time_series_id: self.time_series_id.clone(),
            about_uuid: self.about_uuid.clone(),
            name: self.name.clone(),
            human_name: self.human_name.clone(),
            unit: self.unit.clone(),
            value_type: self.points.values.value_type(),
            time_kind: self.time_kind,
            point_count: self.points.len() as i64,
            first_t: self.points.t.first().copied(),
            last_t: self.points.t.last().copied(),
        }
    }
}

/// Every table a source's time-series store holds, in creation order.
pub fn store_ddl() -> Vec<&'static str> {
    series::DDL
        .iter()
        .chain(chunks::DDL.iter())
        .map(|(_, sql)| *sql)
        .collect()
}

/// Every table the time-series index holds: the rows, and where each
/// source's points are. No points.
pub fn index_ddl() -> Vec<&'static str> {
    series::DDL
        .iter()
        .chain(sources::DDL.iter())
        .map(|(_, sql)| *sql)
        .collect()
}

/// The lookups by id, kept out of the DDL so they change no store's shape.
pub fn indexes() -> Vec<&'static str> {
    series::INDEXES.iter().map(|(_, sql)| *sql).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn strum_and_serde_agree() {
        for &k in TimeKind::VARIANTS {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(TimeKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(TimeKind::parse("interval"), None);
        for &k in ValueType::VARIANTS {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(ValueType::parse(k.as_str()), Some(k));
        }
        assert_eq!(ValueType::parse("bool"), None);
    }

    #[test]
    fn day_numbers_count_from_the_epoch() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("2369-04-14"), Some(145_835));
        assert_eq!(day_number("2369-4-14x"), None);
    }

    #[test]
    fn points_must_ascend_and_match_their_values() {
        assert_eq!(Points::floats([(1, 0.5), (2, 0.6)]).problem(), None);
        assert!(Points::floats([(2, 0.5), (2, 0.6)])
            .problem()
            .unwrap()
            .contains("t 2 is followed by 2"));
        let short = Points {
            t: vec![1, 2],
            values: Values::Integer(vec![1]),
        };
        assert!(short.problem().is_some());
    }

    #[test]
    fn a_row_summarises_its_points() {
        let s = TimeSeries {
            time_series_id: "id".into(),
            source_id: "riker".into(),
            about_uuid: None,
            name: "resting_hr".into(),
            human_name: "Resting HR".into(),
            unit: "/min".into(),
            time_kind: TimeKind::Day,
            points: Points::integers([(5, 48), (9, 50)]),
        };
        let row = s.row(7);
        assert_eq!((row.ts_key, row.point_count), (7, 2));
        assert_eq!((row.first_t, row.last_t), (Some(5), Some(9)));
        assert_eq!(row.value_type, ValueType::Integer);
        let empty = TimeSeries {
            points: Points::floats([]),
            ..s
        };
        assert_eq!(empty.row(1).first_t, None);
    }
}
