//! A time series: one metric of one entity from one source, as values
//! over time. The shape a source's `render_time_series` step writes, the
//! `time_series_index` step stacks and the time-series applet serves.
//! `docs/dev/plans/time_series.md` is the design.

pub mod points;
pub mod series;

use serde::{Deserialize, Serialize};

pub use points::TimeSeriesPointRow;
pub use series::TimeSeriesRow;

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

/// `t` for a [`TimeKind::Day`] point: days since 1970-01-01 of a
/// `YYYY-MM-DD` date. `None` for anything else.
pub fn day_number(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        d.signed_duration_since(chrono::NaiveDate::default())
            .num_days(),
    )
}

/// One reading. A point with neither a value nor a text is kept: it says
/// the source reported the moment and nothing in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub t: i64,
    pub value: Option<f64>,
    /// A reading that is not a number: a sleep stage, a door state.
    pub text: Option<String>,
}

impl Point {
    pub fn value(t: i64, value: f64) -> Self {
        Self {
            t,
            value: Some(value),
            text: None,
        }
    }
}

/// A time series as a render builds it, before any store keys it.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSeries {
    /// Minted by the provider from source id, entity and metric
    /// (`docs/dev/entity_ids.md`), so it is the same in every store and
    /// on every render.
    pub time_series_id: String,
    pub source_id: String,
    /// What was measured: a device, an account, a sensor.
    pub entity: String,
    /// The provider's metric key.
    pub metric: String,
    /// What a chart legend shows.
    pub label: String,
    /// The SI unit `value` is in; empty for a count or a score.
    pub unit: String,
    pub time_kind: TimeKind,
    /// Ascending by `t`, one point per `t`.
    pub points: Vec<Point>,
}

impl TimeSeries {
    /// blake3 over the points in order: what a writer compares to decide
    /// whether a stored time series' points need replacing.
    pub fn points_hash(&self) -> String {
        points_hash(&self.points)
    }

    /// The row for this time series under `ts_key`, a key local to the
    /// store it goes in.
    pub fn row(&self, ts_key: i64) -> TimeSeriesRow {
        TimeSeriesRow {
            ts_key,
            time_series_id: self.time_series_id.clone(),
            source_id: self.source_id.clone(),
            entity: self.entity.clone(),
            metric: self.metric.clone(),
            label: self.label.clone(),
            unit: self.unit.clone(),
            time_kind: self.time_kind,
            points_hash: self.points_hash(),
            point_count: self.points.len() as i64,
            first_t: self.points.first().map(|p| p.t),
            last_t: self.points.last().map(|p| p.t),
        }
    }

    pub fn point_rows(&self, ts_key: i64) -> impl Iterator<Item = TimeSeriesPointRow> + '_ {
        self.points.iter().map(move |p| TimeSeriesPointRow {
            ts_key,
            t: p.t,
            value: p.value,
            text: p.text.clone(),
        })
    }
}

pub fn points_hash(points: &[Point]) -> String {
    let mut h = blake3::Hasher::new();
    for p in points {
        h.update(&p.t.to_le_bytes());
        match p.value {
            Some(v) => h.update(&[1]).update(&v.to_bits().to_le_bytes()),
            None => h.update(&[0]),
        };
        match &p.text {
            Some(s) => h
                .update(&[1])
                .update(&(s.len() as u64).to_le_bytes())
                .update(s.as_bytes()),
            None => h.update(&[0]),
        };
    }
    h.finalize().to_hex().to_string()
}

/// Every table a time-series store holds, in creation order.
pub fn ddl() -> Vec<&'static str> {
    series::DDL
        .iter()
        .chain(points::DDL.iter())
        .map(|(_, sql)| *sql)
        .collect()
}

/// The lookups by id, kept out of [`ddl`] so they change no store's shape.
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
    }

    fn series(points: Vec<Point>) -> TimeSeries {
        TimeSeries {
            time_series_id: "id".into(),
            source_id: "garmin".into(),
            entity: "riker".into(),
            metric: "resting_hr".into(),
            label: "Resting heart rate".into(),
            unit: "1/s".into(),
            time_kind: TimeKind::Day,
            points,
        }
    }

    /// The hash is what decides whether a writer replaces a time series'
    /// points, so every part of a point has to move it, and a value
    /// must not hash like a text that happens to share its bytes.
    #[test]
    fn every_part_of_a_point_moves_the_hash() {
        let base = series(vec![Point::value(1, 0.8), Point::value(2, 0.9)]).points_hash();
        let variants = [
            vec![Point::value(1, 0.8), Point::value(3, 0.9)],
            vec![Point::value(1, 0.8), Point::value(2, 0.95)],
            vec![Point::value(1, 0.8)],
            vec![
                Point::value(1, 0.8),
                Point {
                    t: 2,
                    value: Some(0.9),
                    text: Some("rem".into()),
                },
            ],
            vec![
                Point::value(1, 0.8),
                Point {
                    t: 2,
                    value: None,
                    text: None,
                },
            ],
        ];
        for v in variants {
            assert_ne!(series(v.clone()).points_hash(), base, "{v:?}");
        }
        assert_eq!(
            series(vec![Point::value(1, 0.8), Point::value(2, 0.9)]).points_hash(),
            base
        );
    }

    #[test]
    fn day_numbers_count_from_the_epoch() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("2369-04-14"), Some(145_835));
        assert_eq!(day_number("2369-4-14x"), None);
    }

    #[test]
    fn a_row_summarises_its_points() {
        let s = series(vec![Point::value(5, 1.0), Point::value(9, 2.0)]);
        let row = s.row(7);
        assert_eq!((row.ts_key, row.point_count), (7, 2));
        assert_eq!((row.first_t, row.last_t), (Some(5), Some(9)));
        assert_eq!(row.points_hash, s.points_hash());
        assert!(s.point_rows(7).all(|p| p.ts_key == 7));
        assert_eq!(series(vec![]).row(1).first_t, None);
    }
}
