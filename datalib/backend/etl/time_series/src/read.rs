//! What a reader asks a source's time-series store for: one time
//! series' points in a range, whole or, past a point budget, as buckets
//! of min, max and mean. A chunk that lies inside one bucket is counted
//! from its summary without being decoded. The time-series applet
//! serves these.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use datalib_time_series_schema::{Points, TimeSeriesChunkRow, Values};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{chunk, store};

/// A point as a reader gets it: a number in `value`, or a text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RawPoint {
    pub t: i64,
    pub value: Option<f64>,
    pub text: Option<String>,
}

/// The points of one bucket, summarised: `t` is the first point's.
/// A bucket of text has no numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bucket {
    pub t: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Raw { points: Vec<RawPoint> },
    Buckets { width: i64, buckets: Vec<Bucket> },
}

/// The bucket width that cuts `[from, to]` into at most `budget` buckets:
/// at least 1, so a range narrower than the budget is one point a bucket.
pub fn bucket_width(from: i64, to: i64, budget: i64) -> i64 {
    let span = to.saturating_sub(from).saturating_add(1).max(1);
    let budget = budget.max(1);
    span / budget + i64::from(span % budget != 0)
}

/// Every point of `chunks`, in order.
pub fn decoded(chunks: &[TimeSeriesChunkRow]) -> Result<Points> {
    let mut out: Option<Points> = None;
    for c in chunks {
        let p = chunk::decode(c.encoding, &c.points)?;
        match &mut out {
            None => out = Some(p),
            Some(all) => append(all, p)?,
        }
    }
    Ok(out.unwrap_or(Points {
        t: Vec::new(),
        values: Values::Float(Vec::new()),
    }))
}

fn append(all: &mut Points, more: Points) -> Result<()> {
    match (&mut all.values, more.values) {
        (Values::Integer(a), Values::Integer(b)) => a.extend(b),
        (Values::Float(a), Values::Float(b)) => a.extend(b),
        (Values::Text(a), Values::Text(b)) => a.extend(b),
        (a, b) => anyhow::bail!(
            "a time series with {} chunks and {} chunks",
            a.value_type().as_str(),
            b.value_type().as_str()
        ),
    }
    all.t.extend(more.t);
    Ok(())
}

#[derive(Default)]
struct Acc {
    t: i64,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
    numbers: i64,
    count: i64,
}

impl Acc {
    fn add(&mut self, t: i64, n: i64, min: Option<f64>, max: Option<f64>, sum: Option<f64>) {
        if self.count == 0 || t < self.t {
            self.t = t;
        }
        self.count += n;
        if let (Some(lo), Some(hi), Some(s)) = (min, max, sum) {
            self.min = Some(self.min.map_or(lo, |m| m.min(lo)));
            self.max = Some(self.max.map_or(hi, |m| m.max(hi)));
            self.sum += s;
            self.numbers += n;
        }
    }

    fn bucket(&self) -> Bucket {
        Bucket {
            t: self.t,
            min: self.min,
            max: self.max,
            mean: (self.numbers > 0).then(|| self.sum / self.numbers as f64),
            count: self.count,
        }
    }
}

/// The answer for `[from, to]` from `chunks`, the time series' chunks
/// that overlap it: every point when there are at most `budget`, else
/// `budget` buckets or fewer.
pub fn answer(chunks: &[TimeSeriesChunkRow], from: i64, to: i64, budget: i64) -> Result<Answer> {
    let chunks: Vec<&TimeSeriesChunkRow> = chunks
        .iter()
        .filter(|c| c.first_t <= to && c.last_t >= from)
        .collect();
    let inside = |c: &TimeSeriesChunkRow| c.first_t >= from && c.last_t <= to;
    let in_range = |t: i64| (from..=to).contains(&t);
    let mut decoded: HashMap<usize, Points> = HashMap::new();
    let mut count = 0;
    for (i, c) in chunks.iter().enumerate() {
        if inside(c) {
            count += c.n;
        } else {
            let p = chunk::decode(c.encoding, &c.points)?;
            count += p.t.iter().filter(|&&t| in_range(t)).count() as i64;
            decoded.insert(i, p);
        }
    }

    if count <= budget.max(0) {
        let mut points = Vec::with_capacity(count as usize);
        for (i, c) in chunks.iter().enumerate() {
            let p = match decoded.remove(&i) {
                Some(p) => p,
                None => chunk::decode(c.encoding, &c.points)?,
            };
            for (j, &t) in p.t.iter().enumerate() {
                if in_range(t) {
                    points.push(RawPoint {
                        t,
                        value: p.values.number(j),
                        text: p.values.text(j).map(str::to_string),
                    });
                }
            }
        }
        return Ok(Answer::Raw { points });
    }

    let width = bucket_width(from, to, budget);
    let bucket_of = |t: i64| ((i128::from(t) - i128::from(from)) / i128::from(width)) as i64;
    let mut buckets: BTreeMap<i64, Acc> = BTreeMap::new();
    for (i, c) in chunks.iter().enumerate() {
        if inside(c) && bucket_of(c.first_t) == bucket_of(c.last_t) {
            buckets
                .entry(bucket_of(c.first_t))
                .or_default()
                .add(c.first_t, c.n, c.min, c.max, c.sum);
            continue;
        }
        let p = match decoded.remove(&i) {
            Some(p) => p,
            None => chunk::decode(c.encoding, &c.points)?,
        };
        for (j, &t) in p.t.iter().enumerate() {
            if in_range(t) {
                let v = p.values.number(j);
                buckets.entry(bucket_of(t)).or_default().add(t, 1, v, v, v);
            }
        }
    }
    Ok(Answer::Buckets {
        width,
        buckets: buckets.values().map(Acc::bucket).collect(),
    })
}

/// [`answer`] for `ts_key` in the store `pool` reads.
pub async fn points(
    pool: &SqlitePool,
    ts_key: i64,
    from: i64,
    to: i64,
    budget: i64,
) -> Result<Answer> {
    let chunks = store::chunks(pool, ts_key, from, to).await?;
    answer(&chunks, from, to, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::cut;

    #[test]
    fn a_width_never_makes_more_buckets_than_the_budget() {
        for (from, to, budget) in [(0, 99, 10), (0, 100, 10), (5, 5, 10), (0, 1_000_003, 7)] {
            let w = bucket_width(from, to, budget);
            let buckets = (to - from) / w + 1;
            assert!(buckets <= budget, "{from}..{to} / {budget} → width {w}");
            assert!(w >= 1);
        }
        assert_eq!(bucket_width(0, 9, 100), 1);
        assert_eq!(bucket_width(0, 9, 0), 10);
    }

    /// A bucket made from a chunk's summary must equal the one made from
    /// its points, so the answer cannot depend on where chunks were cut.
    #[test]
    fn buckets_are_the_same_however_the_points_were_chunked() {
        let p = Points::floats((0..1_000).map(|i| (i, ((i * 37) % 101) as f64)));
        let whole = answer(&cut(1, &p, 5_000), 0, 999, 10).unwrap();
        for size in [1, 7, 100, 250] {
            assert_eq!(
                answer(&cut(1, &p, size), 0, 999, 10).unwrap(),
                whole,
                "{size}"
            );
        }
        let Answer::Buckets { width, buckets } = whole else {
            panic!("a thousand points over a budget of ten are bucketed")
        };
        assert_eq!((width, buckets.len()), (100, 10));
        assert_eq!(buckets[1].t, 100);
        assert_eq!(buckets[1].count, 100);
    }

    #[test]
    fn a_range_within_the_budget_comes_back_whole_and_trimmed_to_the_range() {
        let p = Points::integers((0..100).map(|i| (i, i * 2)));
        let Answer::Raw { points } = answer(&cut(1, &p, 30), 10, 19, 50).unwrap() else {
            panic!("ten points under a budget of fifty come back whole")
        };
        assert_eq!(points.len(), 10);
        assert_eq!(
            points[0],
            RawPoint {
                t: 10,
                value: Some(20.0),
                text: None
            }
        );
    }

    #[test]
    fn text_comes_back_as_text_and_its_buckets_only_count() {
        let p = Points::texts((0..20).map(|i| (i, format!("s{}", i % 2))));
        let Answer::Raw { points } = answer(&cut(1, &p, 8), 0, 1, 10).unwrap() else {
            panic!()
        };
        assert_eq!(points[1].text.as_deref(), Some("s1"));
        let Answer::Buckets { buckets, .. } = answer(&cut(1, &p, 8), 0, 19, 2).unwrap() else {
            panic!()
        };
        assert_eq!(
            buckets[0],
            Bucket {
                t: 0,
                min: None,
                max: None,
                mean: None,
                count: 10
            }
        );
    }
}
