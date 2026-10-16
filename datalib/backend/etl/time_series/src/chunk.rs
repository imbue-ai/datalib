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

/// The layouts this build reads. Encoding 2 is encoding 1 with XOR
/// floats and binary-scaled floats added; a writer records the lowest one its blob needs, so an
/// older build refuses only the chunks it cannot read.
pub const ENCODINGS: [i64; 2] = [1, 2];

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
/// Encoding 2 and later.
const XOR_FLOAT: u8 = 4;
/// Encoding 2 and later.
const BINARY: u8 = 5;

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
    let encoded = encode(part, Compression::WhenSmaller);
    TimeSeriesChunkRow {
        ts_key,
        first_t: part.t[0],
        last_t: part.t[part.len() - 1],
        n: part.len() as i64,
        min,
        max,
        sum,
        encoding: encoded.encoding,
        points: encoded.blob,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Never,
    /// Deflate the body, and keep the result only when it is smaller.
    WhenSmaller,
}

/// How floats with no short decimal form are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floats {
    /// 8 bytes each.
    Raw,
    /// Each one's bits XORed with the one before's, the zeros at either
    /// end left out, as Gorilla does.
    Xor,
    /// Integers over a power of two, divided by their common factor,
    /// where every value is one: degrees from a FIT file's semicircles
    /// are integers over 2^29. Raw where they are not.
    Binary,
    /// Whichever makes the smallest blob.
    Smaller,
}

/// What [`values`] writes for floats with no short decimal form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatLayout {
    Raw,
    Xor,
    Binary,
}

/// A blob and the encoding it is in.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    pub encoding: i64,
    pub blob: Vec<u8>,
}

/// The blob for `points`: a byte saying whether the rest is deflated,
/// then the count, the times and the values.
pub fn encode(points: &Points, compression: Compression) -> Encoded {
    encode_with(points, compression, Floats::Smaller)
}

pub fn encode_with(points: &Points, compression: Compression, floats: Floats) -> Encoded {
    use FloatLayout::{Binary, Raw, Xor};
    let layouts: &[FloatLayout] = match (&points.values, floats) {
        (Values::Float(v), _) if as_decimals(v).is_some() => &[Raw],
        (Values::Float(_), Floats::Xor) => &[Xor],
        (Values::Float(_), Floats::Binary) => &[Binary, Raw],
        (Values::Float(_), Floats::Smaller) => &[Raw, Xor, Binary],
        _ => &[Raw],
    };
    let mut encoded = layouts
        .iter()
        .filter_map(|&layout| encode_layout(points, compression, layout));
    let picked = if floats == Floats::Smaller {
        encoded.min_by_key(|e| e.blob.len())
    } else {
        encoded.next()
    };
    picked.expect("every list of layouts ends in one that holds any values")
}

/// `points` with floats in `layout`; `None` when the layout cannot hold
/// them.
fn encode_layout(
    points: &Points,
    compression: Compression,
    layout: FloatLayout,
) -> Option<Encoded> {
    let mut body = Vec::new();
    varint(points.len() as u64, &mut body);
    times(&points.t, &mut body);
    if !values(&points.values, layout, &mut body) {
        return None;
    }
    Some(Encoded {
        encoding: if layout == FloatLayout::Raw { 1 } else { 2 },
        blob: compress(body, compression),
    })
}

fn compress(body: Vec<u8>, compression: Compression) -> Vec<u8> {
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
        ENCODINGS.contains(&encoding),
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
    let mut r = Reader {
        s: body,
        i: 0,
        encoding,
    };
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

/// Write `values`; `false` when `layout` cannot hold them, which only a
/// binary layout of floats that are no integers over a power of two
/// cannot.
fn values(values: &Values, layout: FloatLayout, out: &mut Vec<u8>) -> bool {
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
            None if layout == FloatLayout::Xor => {
                out.push(XOR_FLOAT);
                xor_floats(v, out);
            }
            None if layout == FloatLayout::Binary => {
                let Some((e, factor, scaled)) = as_binary(v) else {
                    return false;
                };
                out.push(BINARY);
                out.push(e);
                varint(factor, out);
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
    true
}

/// Bits, least significant first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u128,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u64, n: u32) {
        if n == 0 {
            return;
        }
        let masked = if n == 64 {
            value
        } else {
            value & ((1 << n) - 1)
        };
        self.acc |= u128::from(masked) << self.bits;
        self.bits += n;
        while self.bits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// The first value's 64 bits; then for each next one, its bits XORed
/// with the one before's: `0` when they are equal, else `1`, then `0`
/// and the XOR's meaningful bits when they fit the window the last
/// change used, or `1`, the count of leading zeros in 5 bits, the count
/// of meaningful bits in 6 (64 written as 0) and those bits.
fn xor_floats(v: &[f64], out: &mut Vec<u8>) {
    let Some(first) = v.first() else { return };
    let mut w = BitWriter::default();
    let mut prev = first.to_bits();
    w.put(prev, 64);
    let mut window: Option<(u32, u32)> = None;
    for x in &v[1..] {
        let bits = x.to_bits();
        let xor = bits ^ prev;
        prev = bits;
        if xor == 0 {
            w.put(0, 1);
            continue;
        }
        w.put(1, 1);
        let lead = xor.leading_zeros().min(31);
        let trail = xor.trailing_zeros();
        match window {
            Some((l, t)) if lead >= l && trail >= t => {
                w.put(0, 1);
                w.put(xor >> t, 64 - l - t);
            }
            _ => {
                let len = 64 - lead - trail;
                w.put(1, 1);
                w.put(u64::from(lead), 5);
                w.put(u64::from(len % 64), 6);
                w.put(xor >> trail, len);
                window = Some((lead, trail));
            }
        }
    }
    out.extend(w.finish());
}

/// The fewest binary places `e` for which every value times 2^e is an
/// integer below 2^53, those integers' greatest common factor, and the
/// integers divided by it; `None` when no `e` up to 64 does, or a value
/// is not finite or is -0.0. Most floats are such an integer at some
/// `e`; the layout pays only where the integers stay small, which
/// [`Floats::Smaller`] measures by encoding.
fn as_binary(v: &[f64]) -> Option<(u8, u64, Vec<i64>)> {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    let e = v.iter().try_fold(0u8, |e, &x| {
        if !x.is_finite() || x.to_bits() == (-0.0f64).to_bits() {
            return None;
        }
        (e..=64).find(|&k| {
            let n = x * 2f64.powi(k.into());
            n.fract() == 0.0 && n.abs() < EXACT
        })
    })?;
    let scale = 2f64.powi(e.into());
    let ints: Vec<i64> = v.iter().map(|&x| (x * scale) as i64).collect();
    let factor = ints
        .iter()
        .fold(0u64, |g, &n| gcd(g, n.unsigned_abs()))
        .max(1);
    let scaled: Vec<i64> = ints.iter().map(|&n| n / factor as i64).collect();
    let back = |q: i64| ((q * factor as i64) as f64 / scale).to_bits();
    v.iter()
        .zip(&scaled)
        .all(|(x, &q)| back(q) == x.to_bits())
        .then_some((e, factor, scaled))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
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
    encoding: i64,
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

    fn xor_floats(&mut self, n: usize) -> Result<Vec<f64>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut r = BitReader {
            s: &self.s[self.i..],
            bit: 0,
        };
        let mut prev = r.get(64)?;
        let mut out = Vec::with_capacity(n);
        out.push(f64::from_bits(prev));
        let mut window: Option<(u32, u32)> = None;
        for _ in 1..n {
            if r.get(1)? == 1 {
                let (lead, trail) = if r.get(1)? == 0 {
                    window.context("an XOR float reusing a window before any")?
                } else {
                    let lead = r.get(5)? as u32;
                    let len = match r.get(6)? as u32 {
                        0 => 64,
                        len => len,
                    };
                    ensure!(lead + len <= 64, "an XOR float window past 64 bits");
                    let w = (lead, 64 - lead - len);
                    window = Some(w);
                    w
                };
                prev ^= r.get(64 - lead - trail)? << trail;
            }
            out.push(f64::from_bits(prev));
        }
        self.i += r.bit.div_ceil(8);
        Ok(out)
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
            XOR_FLOAT => {
                ensure!(self.encoding >= 2, "XOR floats in an encoding-1 chunk");
                Values::Float(self.xor_floats(n)?)
            }
            BINARY => {
                ensure!(self.encoding >= 2, "binary floats in an encoding-1 chunk");
                let e = self.byte()?;
                ensure!(e <= 64, "a chunk scaled past 64 binary places");
                let factor = i64::try_from(self.varint()?).context("a chunk's factor too large")?;
                let scale = 2f64.powi(e.into());
                let values = self
                    .deltas(n)?
                    .into_iter()
                    .map(|q| {
                        q.checked_mul(factor)
                            .map(|x| x as f64 / scale)
                            .context("a chunk's value past 64 bits")
                    })
                    .collect::<Result<_>>()?;
                Values::Float(values)
            }
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

struct BitReader<'a> {
    s: &'a [u8],
    bit: usize,
}

impl BitReader<'_> {
    fn get(&mut self, n: u32) -> Result<u64> {
        let mut out = 0u64;
        for k in 0..n {
            let byte = *self
                .s
                .get(self.bit / 8)
                .context("a chunk that ends early")?;
            out |= u64::from((byte >> (self.bit % 8)) & 1) << k;
            self.bit += 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_WAY: [(Compression, Floats); 8] = [
        (Compression::Never, Floats::Raw),
        (Compression::Never, Floats::Xor),
        (Compression::Never, Floats::Binary),
        (Compression::Never, Floats::Smaller),
        (Compression::WhenSmaller, Floats::Raw),
        (Compression::WhenSmaller, Floats::Xor),
        (Compression::WhenSmaller, Floats::Binary),
        (Compression::WhenSmaller, Floats::Smaller),
    ];

    fn round_trip(p: &Points) -> Vec<u8> {
        for (c, f) in EVERY_WAY {
            let e = encode_with(p, c, f);
            assert_eq!(&decode(e.encoding, &e.blob).unwrap(), p, "{c:?} {f:?}");
        }
        encode_with(p, Compression::Never, Floats::Raw).blob
    }

    /// Degrees along a walk: no short decimal form, close to the one
    /// before.
    fn wandering(n: i64) -> Points {
        Points::floats(
            (0..n).map(|i| (i, 51.5 + (i as f64 * 0.731).sin() * 1e-4 + i as f64 * 3e-6)),
        )
    }

    /// XOR floats are written only where they make the smaller blob, and
    /// a chunk is labelled encoding 2 only when it holds them.
    #[test]
    fn xor_floats_are_chosen_when_smaller_and_labelled_so() {
        let walk = wandering(1_000);
        let raw = encode_with(&walk, Compression::Never, Floats::Raw);
        let best = encode(&walk, Compression::Never);
        assert_eq!(raw.encoding, 1);
        assert_eq!(best.encoding, 2);
        assert!(
            best.blob.len() < raw.blob.len(),
            "{} vs {}",
            best.blob.len(),
            raw.blob.len()
        );
        let decimals = encode(&Points::floats([(1, 0.5), (2, 0.25)]), Compression::Never);
        assert_eq!(decimals.encoding, 1);
        let constant = encode(
            &Points::floats((0..100).map(|i| (i, std::f64::consts::PI))),
            Compression::Never,
        );
        assert_eq!(
            decode(constant.encoding, &constant.blob).unwrap().len(),
            100
        );
        round_trip(&walk);
        round_trip(&Points::floats([
            (1, f64::MAX),
            (2, f64::MIN_POSITIVE),
            (3, -0.0),
            (4, 1e-300),
        ]));
    }

    /// Degrees from semicircles are integers over 2^29 with a factor of
    /// 45 in common, so they cost what the semicircles would.
    #[test]
    fn degrees_from_semicircles_cost_what_the_semicircles_cost() {
        let semis: Vec<(i64, i64)> = (0..1_000)
            .map(|i| (i, 614_000_000 + (i * 37) % 900))
            .collect();
        let degrees = Points::floats(
            semis
                .iter()
                .map(|&(t, s)| (t, s as f64 * (180.0 / 2_147_483_648.0))),
        );
        let ints = round_trip(&Points::integers(semis.clone()));
        round_trip(&degrees);
        let e = encode(&degrees, Compression::Never);
        assert_eq!(e.encoding, 2);
        assert!(
            e.blob.len() <= ints.len() + 3,
            "{} vs {}",
            e.blob.len(),
            ints.len()
        );
        assert_eq!(as_binary(&[0.5, 1.5]), Some((1, 1, vec![1, 3])));
        assert_eq!(
            as_binary(&[0.1]),
            Some((55, 3_602_879_701_896_397, vec![1])),
            "0.1 is exactly that integer over 2^55"
        );
        assert_eq!(as_binary(&[1e-30]), None, "past 64 binary places");
        assert_eq!(as_binary(&[-0.0]), None);
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
        let other = f64::from_bits(0xfff8_0000_0000_0007);
        for (c, f) in EVERY_WAY {
            let e = encode_with(&Points::floats([(1, nan), (2, other), (3, nan)]), c, f);
            let Values::Float(v) = decode(e.encoding, &e.blob).unwrap().values else {
                panic!()
            };
            let bits: Vec<u64> = v.iter().map(|x| x.to_bits()).collect();
            assert_eq!(
                bits,
                [nan.to_bits(), other.to_bits(), nan.to_bits()],
                "{c:?} {f:?}"
            );
        }
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
        let good = encode_with(
            &Points::texts((0..50).map(|i| (i, format!("s{}", i % 3)))),
            Compression::Never,
            Floats::Raw,
        )
        .blob;
        for len in 0..good.len() {
            assert!(decode(1, &good[..len]).is_err(), "cut at {len}");
        }
        let xor = encode_with(&wandering(50), Compression::Never, Floats::Xor);
        assert_eq!(xor.encoding, 2);
        for len in 0..xor.blob.len() {
            assert!(decode(2, &xor.blob[..len]).is_err(), "XOR cut at {len}");
        }
        assert!(
            decode(1, &xor.blob).is_err(),
            "an encoding-1 chunk cannot hold XOR floats"
        );
        assert!(decode(3, &xor.blob).is_err());
        assert!(decode(1, &[9, 0]).is_err());
        let mut lying = vec![PLAIN];
        varint(u64::MAX, &mut lying);
        assert!(decode(1, &lying).is_err());
    }
}
