//! A time series is stored as chunks: runs of up to [`CHUNK_POINTS`]
//! points, cut by position from its first point, each encoded into one
//! blob. Cutting by position means an append rewrites only the last
//! chunk. The layout, and the measurements that chose it, are in
//! `docs/dev/time_series.md` §"The chunk encoding".

use anyhow::{bail, ensure, Context, Result};
use datalib_time_series_schema::{Points, TimeSeriesChunkRow, Values};
use std::io::{Read, Write};

/// Points per chunk (`datalib/backend/etl/time_series/measure/`).
pub const CHUNK_POINTS: usize = 1024;

/// The layout [`encode`] writes. A reader refuses any other.
pub const ENCODING: i64 = 1;

/// A count a blob may claim before a reader calls it corrupt.
const MAX_POINTS: u64 = 1 << 24;

/// The most decimals a float series is scaled by to make it integers.
/// 10^15 is the largest power of ten whose products stay exact.
const POWERS_OF_TEN: [f64; 16] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
];

const PLAIN: u8 = 0;
const DEFLATED: u8 = 1;

const INTEGER: u8 = 0;
const DECIMAL: u8 = 1;
const RAW_FLOAT: u8 = 2;
const TEXT: u8 = 3;

/// `points` cut into chunk rows under `ts_key`.
pub fn cut(ts_key: i64, points: &Points, size: usize) -> Vec<TimeSeriesChunkRow> {
    let size = size.max(1);
    (0..points.len())
        .step_by(size)
        .map(|start| {
            let part = points.slice(start..(start + size).min(points.len()));
            chunk_row(ts_key, &part)
        })
        .collect()
}

fn chunk_row(ts_key: i64, part: &Points) -> TimeSeriesChunkRow {
    let numbers: Vec<f64> = (0..part.len())
        .filter_map(|i| part.values.number(i))
        .collect();
    let (min, max, sum) = if numbers.is_empty() {
        (None, None, None)
    } else {
        (
            Some(numbers.iter().copied().fold(f64::INFINITY, f64::min)),
            Some(numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
            Some(numbers.iter().sum()),
        )
    };
    TimeSeriesChunkRow {
        ts_key,
        first_t: part.t[0],
        last_t: part.t[part.len() - 1],
        n: part.len() as i64,
        min,
        max,
        sum,
        encoding: ENCODING,
        points: encode(part, Compression::WhenSmaller),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Never,
    /// Deflate the body, and keep the result only when it is smaller.
    WhenSmaller,
}

/// The blob for `points`: a byte saying whether the rest is deflated,
/// then the count, the times and the values.
pub fn encode(points: &Points, compression: Compression) -> Vec<u8> {
    let mut body = Vec::new();
    varint(points.len() as u64, &mut body);
    times(&points.t, &mut body);
    values(&points.values, &mut body);
    if compression == Compression::WhenSmaller {
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        // Writing to a Vec cannot fail.
        if z.write_all(&body).is_ok() {
            if let Ok(deflated) = z.finish() {
                if deflated.len() < body.len() {
                    let mut out = Vec::with_capacity(deflated.len() + 1);
                    out.push(DEFLATED);
                    out.extend(deflated);
                    return out;
                }
            }
        }
    }
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(PLAIN);
    out.extend(body);
    out
}

pub fn decode(encoding: i64, blob: &[u8]) -> Result<Points> {
    ensure!(
        encoding == ENCODING,
        "a chunk in encoding {encoding}, which this build cannot read"
    );
    let (&flag, rest) = blob.split_first().context("an empty chunk")?;
    let inflated;
    let body = match flag {
        PLAIN => rest,
        DEFLATED => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(rest)
                .read_to_end(&mut out)
                .context("inflate a chunk")?;
            inflated = out;
            &inflated
        }
        other => bail!("a chunk with compression byte {other}"),
    };
    let mut r = Reader { s: body, i: 0 };
    let n = r.varint()?;
    ensure!(n <= MAX_POINTS, "a chunk claiming {n} points");
    let n = n as usize;
    let t = r.times(n)?;
    let values = r.values(n)?;
    ensure!(r.i == body.len(), "a chunk with bytes past its values");
    Ok(Points { t, values })
}

fn zigzag(x: i64) -> u64 {
    ((x << 1) ^ (x >> 63)) as u64
}

fn unzigzag(x: u64) -> i64 {
    ((x >> 1) as i64) ^ -((x & 1) as i64)
}

fn varint(mut x: u64, out: &mut Vec<u8>) {
    while x >= 0x80 {
        out.push((x as u8) | 0x80);
        x >>= 7;
    }
    out.push(x as u8);
}

/// `xs` at the narrowest width that holds every one: a byte for the
/// width, then the bits, least significant first. All zeros take one
/// byte whatever their number.
fn pack(xs: &[u64], out: &mut Vec<u8>) {
    let width = xs.iter().map(|x| 64 - x.leading_zeros()).max().unwrap_or(0);
    out.push(width as u8);
    if width == 0 {
        return;
    }
    let mut acc: u128 = 0;
    let mut bits = 0;
    for &x in xs {
        acc |= u128::from(x) << bits;
        bits += width;
        while bits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push(acc as u8);
    }
}

/// The first time, the first gap, then each gap's change from the one
/// before: evenly spaced times pack to nothing.
fn times(t: &[i64], out: &mut Vec<u8>) {
    let Some(&first) = t.first() else { return };
    varint(zigzag(first), out);
    if t.len() < 2 {
        return;
    }
    let gaps: Vec<i64> = t.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
    varint(zigzag(gaps[0]), out);
    let changes: Vec<u64> = gaps
        .windows(2)
        .map(|g| zigzag(g[1].wrapping_sub(g[0])))
        .collect();
    pack(&changes, out);
}

/// The first value, then each one's change from the one before.
fn deltas(v: &[i64], out: &mut Vec<u8>) {
    let Some(&first) = v.first() else { return };
    varint(zigzag(first), out);
    let changes: Vec<u64> = v
        .windows(2)
        .map(|w| zigzag(w[1].wrapping_sub(w[0])))
        .collect();
    pack(&changes, out);
}

fn values(values: &Values, out: &mut Vec<u8>) {
    match values {
        Values::Integer(v) => {
            out.push(INTEGER);
            deltas(v, out);
        }
        Values::Float(v) => match as_decimals(v) {
            Some((e, scaled)) => {
                out.push(DECIMAL);
                out.push(e);
                deltas(&scaled, out);
            }
            None => {
                out.push(RAW_FLOAT);
                for x in v {
                    out.extend(x.to_le_bytes());
                }
            }
        },
        Values::Text(v) => {
            out.push(TEXT);
            let mut dict: Vec<&str> = Vec::new();
            let mut index: std::collections::HashMap<&str, u64> = Default::default();
            let codes: Vec<u64> = v
                .iter()
                .map(|s| {
                    *index.entry(s.as_str()).or_insert_with(|| {
                        dict.push(s);
                        dict.len() as u64 - 1
                    })
                })
                .collect();
            varint(dict.len() as u64, out);
            for s in dict {
                varint(s.len() as u64, out);
                out.extend(s.as_bytes());
            }
            pack(&codes, out);
        }
    }
}

/// The fewest decimals `e` for which every value is an integer over
/// 10^e that reads back to the same bits, and those integers; `None`
/// when no `e` up to 15 does, or a value is not finite.
fn as_decimals(v: &[f64]) -> Option<(u8, Vec<i64>)> {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    'scale: for (e, &scale) in POWERS_OF_TEN.iter().enumerate() {
        let mut out = Vec::with_capacity(v.len());
        for &x in v {
            if !x.is_finite() {
                return None;
            }
            let n = (x * scale).round();
            if n.abs() >= EXACT || ((n as i64) as f64 / scale).to_bits() != x.to_bits() {
                continue 'scale;
            }
            out.push(n as i64);
        }
        return Some((e as u8, out));
    }
    None
}

struct Reader<'a> {
    s: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.s.get(self.i).context("a chunk that ends early")?;
        self.i += 1;
        Ok(b)
    }

    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .i
            .checked_add(len)
            .filter(|&e| e <= self.s.len())
            .context("a chunk that ends early")?;
        let out = &self.s[self.i..end];
        self.i = end;
        Ok(out)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut x = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            x |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(x);
            }
        }
        bail!("a chunk with a varint past 64 bits")
    }

    fn unpack(&mut self, n: usize) -> Result<Vec<u64>> {
        let width = u32::from(self.byte()?);
        ensure!(width <= 64, "a chunk packed {width} bits wide");
        if width == 0 {
            return Ok(vec![0; n]);
        }
        let bytes = self.take((n * width as usize).div_ceil(8))?;
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let mut out = Vec::with_capacity(n);
        let mut acc: u128 = 0;
        let mut bits = 0;
        let mut it = bytes.iter();
        for _ in 0..n {
            while bits < width {
                acc |= u128::from(*it.next().context("a chunk that ends early")?) << bits;
                bits += 8;
            }
            out.push((acc as u64) & mask);
            acc >>= width;
            bits -= width;
        }
        Ok(out)
    }

    fn times(&mut self, n: usize) -> Result<Vec<i64>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut t = Vec::with_capacity(n);
        t.push(unzigzag(self.varint()?));
        if n == 1 {
            return Ok(t);
        }
        let mut gap = unzigzag(self.varint()?);
        t.push(t[0].wrapping_add(gap));
        for change in self.unpack(n - 2)? {
            gap = gap.wrapping_add(unzigzag(change));
            t.push(t[t.len() - 1].wrapping_add(gap));
        }
        Ok(t)
    }

    fn deltas(&mut self, n: usize) -> Result<Vec<i64>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut v = Vec::with_capacity(n);
        v.push(unzigzag(self.varint()?));
        for change in self.unpack(n - 1)? {
            v.push(v[v.len() - 1].wrapping_add(unzigzag(change)));
        }
        Ok(v)
    }

    fn values(&mut self, n: usize) -> Result<Values> {
        Ok(match self.byte()? {
            INTEGER => Values::Integer(self.deltas(n)?),
            DECIMAL => {
                let e = usize::from(self.byte()?);
                let scale = *POWERS_OF_TEN
                    .get(e)
                    .context("a chunk scaled past 15 decimals")?;
                Values::Float(
                    self.deltas(n)?
                        .into_iter()
                        .map(|x| x as f64 / scale)
                        .collect(),
                )
            }
            RAW_FLOAT => Values::Float(
                self.take(n.checked_mul(8).context("a chunk too long")?)?
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| f64::from_le_bytes(*b))
                    .collect(),
            ),
            TEXT => {
                let size = self.varint()?;
                ensure!(size <= MAX_POINTS, "a chunk claiming {size} distinct texts");
                let mut dict = Vec::with_capacity(size as usize);
                for _ in 0..size {
                    let len = self.varint()?;
                    let len = usize::try_from(len).context("a chunk text too long")?;
                    let s = std::str::from_utf8(self.take(len)?).context("a chunk text")?;
                    dict.push(s.to_string());
                }
                let texts = self
                    .unpack(n)?
                    .into_iter()
                    .map(|c| {
                        dict.get(c as usize)
                            .cloned()
                            .context("a chunk naming a text it does not hold")
                    })
                    .collect::<Result<_>>()?;
                Values::Text(texts)
            }
            other => bail!("a chunk with value tag {other}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(p: &Points) -> Vec<u8> {
        for c in [Compression::Never, Compression::WhenSmaller] {
            let blob = encode(p, c);
            assert_eq!(&decode(ENCODING, &blob).unwrap(), p, "{c:?}");
        }
        encode(p, Compression::Never)
    }

    #[test]
    fn every_kind_of_value_round_trips() {
        round_trip(&Points::integers([]));
        round_trip(&Points::integers([(7, -3)]));
        round_trip(&Points::integers([
            (i64::MIN, i64::MAX),
            (0, i64::MIN),
            (i64::MAX, 0),
        ]));
        round_trip(&Points::floats([
            (1, 0.1),
            (2, 80.5),
            (3, 0.045),
            (9, -2.0),
        ]));
        round_trip(&Points::floats([
            (2, f64::INFINITY),
            (3, -0.0),
            (4, std::f64::consts::PI),
        ]));
        round_trip(&Points::texts([
            (1, "BALANCED".into()),
            (2, "rem \"deep\"".into()),
            (3, "BALANCED".into()),
            (4, String::new()),
        ]));
    }

    /// NaN never equals itself, so compare its bits.
    #[test]
    fn a_nan_keeps_its_bits() {
        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let blob = encode(&Points::floats([(1, nan)]), Compression::Never);
        let Values::Float(v) = decode(ENCODING, &blob).unwrap().values else {
            panic!()
        };
        assert_eq!(v[0].to_bits(), nan.to_bits());
    }

    /// Evenly spaced times and a constant value cost their first entry
    /// and a byte for each stream, whatever the count.
    #[test]
    fn regular_times_and_a_constant_value_take_almost_nothing() {
        let short = round_trip(&Points::integers((0..200).map(|i| (1_000 * i, 60))));
        let long = round_trip(&Points::integers((0..1_000).map(|i| (1_000 * i, 60))));
        assert_eq!(short.len(), long.len());
        assert!(long.len() < 12, "{} bytes", long.len());
    }

    /// Readings with two decimals are stored as integers, so they cost
    /// what the same integers cost.
    #[test]
    fn decimal_floats_cost_what_their_integers_cost() {
        let ints = Points::integers((0..500).map(|i| (i, 2_000 + (i * 7) % 13)));
        let floats = Points::floats((0..500).map(|i| (i, (2_000 + (i * 7) % 13) as f64 / 100.0)));
        let (a, b) = (round_trip(&ints), round_trip(&floats));
        assert_eq!(b.len(), a.len() + 1, "one byte more, for the scale");
    }

    #[test]
    fn a_series_is_cut_by_position_and_an_append_moves_only_the_last_chunk() {
        let p = Points::floats((0..10).map(|i| (i, i as f64)));
        let chunks = cut(3, &p, 4);
        assert_eq!(
            chunks
                .iter()
                .map(|c| (c.first_t, c.last_t, c.n))
                .collect::<Vec<_>>(),
            [(0, 3, 4), (4, 7, 4), (8, 9, 2)]
        );
        assert_eq!(
            (chunks[1].min, chunks[1].max, chunks[1].sum),
            (Some(4.0), Some(7.0), Some(22.0))
        );
        let longer = cut(3, &Points::floats((0..11).map(|i| (i, i as f64))), 4);
        assert_eq!(longer[..2], chunks[..2]);
        assert_ne!(longer[2], chunks[2]);
        let texts = cut(1, &Points::texts([(1, "a".into())]), 4);
        assert_eq!((texts[0].min, texts[0].sum), (None, None));
    }

    #[test]
    fn a_corrupt_chunk_is_an_error_not_a_panic() {
        let good = encode(
            &Points::texts((0..50).map(|i| (i, format!("s{}", i % 3)))),
            Compression::Never,
        );
        for len in 0..good.len() {
            assert!(decode(ENCODING, &good[..len]).is_err(), "cut at {len}");
        }
        assert!(decode(2, &good).is_err());
        assert!(decode(ENCODING, &[9, 0]).is_err());
        let mut lying = vec![PLAIN];
        varint(u64::MAX, &mut lying);
        assert!(decode(ENCODING, &lying).is_err());
    }
}
