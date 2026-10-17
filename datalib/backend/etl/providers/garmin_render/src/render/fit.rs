//! Enough of the FIT protocol (garmin.com/fit) to read an activity's
//! `record` messages, the per-second samples, as raw integers: file
//! header, definition and data messages, compressed-timestamp headers,
//! both byte orders; developer fields, strings, arrays and floats are
//! skipped. What a record field means is [`RECORD_FIELDS`].

use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};

/// FIT's global message number for a `record`.
const RECORD: u16 = 20;
/// The field number every message's timestamp has.
const TIMESTAMP: u8 = 253;
/// FIT time counts seconds from 1989-12-31T00:00:00Z.
const FIT_EPOCH_S: i64 = 631_065_600;

/// One record: its time in unix milliseconds, and each integer field
/// that held a valid value, by field number.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub t_ms: i64,
    pub fields: BTreeMap<u8, i64>,
}

struct Definition {
    global: u16,
    big_endian: bool,
    /// (field number, size, base type)
    fields: Vec<(u8, u8, u8)>,
    developer_bytes: usize,
}

/// Every record of a FIT file that carries a timestamp, in file order.
pub fn records(buf: &[u8]) -> Result<Vec<Record>> {
    ensure!(buf.len() >= 12, "a FIT file of {} bytes", buf.len());
    let header = usize::from(buf[0]);
    ensure!(
        buf.get(8..12) == Some(b".FIT".as_slice()),
        "no .FIT signature"
    );
    let data = u32::from_le_bytes(buf[4..8].try_into()?) as usize;
    let end = header
        .checked_add(data)
        .filter(|&e| e <= buf.len())
        .context("a FIT file shorter than its header says")?;
    let mut defs: BTreeMap<u8, Definition> = BTreeMap::new();
    let mut out = Vec::new();
    let mut last_ts: Option<i64> = None;
    let mut pos = header;
    while pos < end {
        let h = buf[pos];
        pos += 1;
        if h & 0x80 != 0 {
            let local = (h >> 5) & 0x3;
            let d = defs
                .get(&local)
                .with_context(|| format!("data for undefined local message {local}"))?;
            let (mut vals, next) = data_message(buf, pos, end, d)?;
            pos = next;
            if let Some(last) = last_ts {
                let mut ts = (last & !0x1F) + i64::from(h & 0x1F);
                if ts < last {
                    ts += 0x20;
                }
                vals.insert(TIMESTAMP, ts);
                last_ts = Some(ts);
            }
            if d.global == RECORD {
                push(&mut out, vals);
            }
        } else if h & 0x40 != 0 {
            let (d, next) = definition(buf, pos, end, h & 0x20 != 0)?;
            pos = next;
            defs.insert(h & 0x0F, d);
        } else {
            let local = h & 0x0F;
            let d = defs
                .get(&local)
                .with_context(|| format!("data for undefined local message {local}"))?;
            let (vals, next) = data_message(buf, pos, end, d)?;
            pos = next;
            if let Some(&ts) = vals.get(&TIMESTAMP) {
                last_ts = Some(ts);
            }
            if d.global == RECORD {
                push(&mut out, vals);
            }
        }
    }
    Ok(out)
}

fn push(out: &mut Vec<Record>, mut fields: BTreeMap<u8, i64>) {
    if let Some(ts) = fields.remove(&TIMESTAMP) {
        out.push(Record {
            t_ms: (ts + FIT_EPOCH_S) * 1_000,
            fields,
        });
    }
}

fn definition(buf: &[u8], pos: usize, end: usize, developer: bool) -> Result<(Definition, usize)> {
    let fixed = buf.get(pos..pos + 5).filter(|_| pos + 5 <= end);
    let fixed = fixed.context("a definition past the end")?;
    let big_endian = fixed[1] != 0;
    let global = if big_endian {
        u16::from_be_bytes([fixed[2], fixed[3]])
    } else {
        u16::from_le_bytes([fixed[2], fixed[3]])
    };
    let n = usize::from(fixed[4]);
    let mut at = pos + 5;
    let raw = buf
        .get(at..at + 3 * n)
        .filter(|_| at + 3 * n <= end)
        .context("a definition's fields past the end")?;
    let fields = raw.chunks(3).map(|f| (f[0], f[1], f[2])).collect();
    at += 3 * n;
    let mut developer_bytes = 0;
    if developer {
        let nd = usize::from(*buf.get(at).context("a definition past the end")?);
        let dev = buf
            .get(at + 1..at + 1 + 3 * nd)
            .filter(|_| at + 1 + 3 * nd <= end)
            .context("a definition's developer fields past the end")?;
        developer_bytes = dev.chunks(3).map(|f| usize::from(f[1])).sum();
        at += 1 + 3 * nd;
    }
    Ok((
        Definition {
            global,
            big_endian,
            fields,
            developer_bytes,
        },
        at,
    ))
}

/// The size, signedness and invalid value of an integer base type;
/// `None` for a string, a float or a byte array.
fn integer_type(base: u8) -> Option<(usize, bool, u64)> {
    Some(match base & 0x1F {
        0x00 | 0x02 => (1, false, 0xFF),
        0x01 => (1, true, 0x7F),
        0x03 => (2, true, 0x7FFF),
        0x04 => (2, false, 0xFFFF),
        0x05 => (4, true, 0x7FFF_FFFF),
        0x06 => (4, false, 0xFFFF_FFFF),
        0x0A => (1, false, 0),
        0x0B => (2, false, 0),
        0x0C => (4, false, 0),
        0x0E => (8, true, 0x7FFF_FFFF_FFFF_FFFF),
        0x0F => (8, false, u64::MAX),
        0x10 => (8, false, 0),
        _ => return None,
    })
}

fn data_message(
    buf: &[u8],
    mut pos: usize,
    end: usize,
    d: &Definition,
) -> Result<(BTreeMap<u8, i64>, usize)> {
    let mut vals = BTreeMap::new();
    for &(num, size, base) in &d.fields {
        let size = usize::from(size);
        let raw = buf
            .get(pos..pos + size)
            .filter(|_| pos + size <= end)
            .context("a data message past the end")?;
        pos += size;
        let Some((width, signed, invalid)) = integer_type(base) else {
            continue;
        };
        if width != size {
            continue; // an array
        }
        let mut bytes = [0u8; 8];
        if d.big_endian {
            bytes[8 - width..].copy_from_slice(raw);
        } else {
            bytes[..width].copy_from_slice(raw);
        }
        let u = if d.big_endian {
            u64::from_be_bytes(bytes)
        } else {
            u64::from_le_bytes(bytes)
        };
        if u == invalid {
            continue;
        }
        let shift = 64 - 8 * width as u32;
        let v = if signed {
            ((u << shift) as i64) >> shift
        } else if u > i64::MAX as u64 {
            continue;
        } else {
            u as i64
        };
        vals.insert(num, v);
    }
    pos = pos
        .checked_add(d.developer_bytes)
        .filter(|&p| p <= end)
        .context("developer fields past the end")?;
    Ok((vals, pos))
}

/// How a record field's raw integer becomes a value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scale {
    /// The integer is the value.
    Integer,
    /// `(raw - offset * scale) / scale`, taken in that order so a
    /// reading with one decimal stays one (`docs/dev/time_series.md`).
    Decimal { scale: f64, offset: f64 },
    /// A latitude or longitude in semicircles, as degrees.
    Semicircles,
}

/// A `record` field datalib keeps, from the FIT profile.
pub struct RecordField {
    pub num: u8,
    pub name: &'static str,
    pub human_name: &'static str,
    /// UCUM.
    pub unit: &'static str,
    pub scale: Scale,
    /// A field that carries the same reading at more range: when the
    /// record has it, this one is left out.
    pub superseded_by: Option<u8>,
}

const fn field(
    num: u8,
    name: &'static str,
    human_name: &'static str,
    unit: &'static str,
    scale: Scale,
) -> RecordField {
    RecordField {
        num,
        name,
        human_name,
        unit,
        scale,
        superseded_by: None,
    }
}

const fn decimal(scale: f64, offset: f64) -> Scale {
    Scale::Decimal { scale, offset }
}

#[rustfmt::skip]
pub const RECORD_FIELDS: &[RecordField] = &[
    field(0, "position_lat", "Latitude", "deg", Scale::Semicircles),
    field(1, "position_long", "Longitude", "deg", Scale::Semicircles),
    RecordField { superseded_by: Some(78), ..field(2, "altitude", "Altitude", "m", decimal(5.0, 500.0)) },
    field(3, "heart_rate", "Heart rate", "{beats}/min", Scale::Integer),
    field(4, "cadence", "Cadence", "{cycles}/min", Scale::Integer),
    field(5, "distance", "Distance", "m", decimal(100.0, 0.0)),
    RecordField { superseded_by: Some(73), ..field(6, "speed", "Speed", "m/s", decimal(1000.0, 0.0)) },
    field(7, "power", "Power", "W", Scale::Integer),
    field(13, "temperature", "Temperature", "Cel", Scale::Integer),
    field(39, "vertical_oscillation", "Vertical oscillation", "mm", decimal(10.0, 0.0)),
    field(41, "stance_time_percent", "Ground contact balance", "%", decimal(100.0, 0.0)),
    field(73, "enhanced_speed", "Speed", "m/s", decimal(1000.0, 0.0)),
    field(78, "enhanced_altitude", "Altitude", "m", decimal(5.0, 500.0)),
    field(83, "vertical_ratio", "Vertical ratio", "%", decimal(100.0, 0.0)),
    field(85, "step_length", "Step length", "mm", decimal(10.0, 0.0)),
];

/// A raw integer as the value it stands for.
pub fn decimal_value(raw: i64, scale: Scale) -> f64 {
    match scale {
        Scale::Integer => raw as f64,
        Scale::Decimal { scale, offset } => (raw as f64 - offset * scale) / scale,
        Scale::Semicircles => raw as f64 * (180.0 / 2_147_483_648.0),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A FIT file of `records`, each `(seconds since the FIT epoch,
    /// heart rate, speed in mm/s)`; the first carries a full timestamp,
    /// the rest compressed ones, as a watch writes them.
    pub fn fit_file(records: &[(u32, u8, u16)]) -> Vec<u8> {
        let mut data = Vec::new();
        // Local 0: a record with a timestamp, heart rate and speed.
        data.extend([0x40, 0, 0, 20, 0, 3, 253, 4, 0x86, 3, 1, 0x02, 6, 2, 0x84]);
        // Local 1: a record with heart rate and speed, timed by the header.
        data.extend([0x41, 0, 0, 20, 0, 2, 3, 1, 0x02, 6, 2, 0x84]);
        for (i, &(ts, hr, speed)) in records.iter().enumerate() {
            if i == 0 {
                data.push(0x00);
                data.extend(ts.to_le_bytes());
            } else {
                data.push(0x80 | (1 << 5) | (ts & 0x1F) as u8);
            }
            data.push(hr);
            data.extend(speed.to_le_bytes());
        }
        let mut out = vec![12, 0x10, 0, 0];
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(b".FIT");
        out.extend(data);
        out.extend([0, 0]); // the CRC, which this reader does not check
        out
    }

    #[test]
    fn records_read_back_with_full_and_compressed_timestamps() {
        let buf = fit_file(&[
            (1_000_000_030, 140, 3_210),
            (1_000_000_031, 0xFF, 3_300),
            (1_000_000_033, 143, 0xFFFF),
        ]);
        let recs = records(&buf).unwrap();
        let base = (1_000_000_030 + FIT_EPOCH_S) * 1_000;
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].t_ms, base);
        assert_eq!(recs[1].t_ms, base + 1_000);
        assert_eq!(recs[2].t_ms, base + 3_000);
        assert_eq!(recs[0].fields, BTreeMap::from([(3, 140), (6, 3_210)]));
        assert_eq!(
            recs[1].fields,
            BTreeMap::from([(6, 3_300)]),
            "0xFF is no heart rate"
        );
        assert_eq!(
            recs[2].fields,
            BTreeMap::from([(3, 143)]),
            "0xFFFF is no speed"
        );
    }

    /// A compressed timestamp holds only the low five bits, so one that
    /// is below the last time's means the 32-second window rolled over.
    #[test]
    fn a_compressed_timestamp_rolls_over_its_window() {
        let recs = records(&fit_file(&[(1_000_000_030, 1, 1), (1_000_000_033, 2, 2)])).unwrap();
        assert_eq!(recs[1].t_ms - recs[0].t_ms, 3_000);
    }

    #[test]
    fn a_truncated_or_foreign_file_is_an_error() {
        let buf = fit_file(&[(1_000_000_030, 140, 3_210), (1_000_000_031, 141, 3_300)]);
        for len in [0, 11, 20, buf.len() - 4] {
            assert!(records(&buf[..len]).is_err(), "cut at {len}");
        }
        assert!(records(b"PK\x03\x04 not a fit file at all").is_err());
    }

    #[test]
    fn scaled_fields_keep_their_decimals() {
        assert_eq!(decimal_value(2_501, decimal(5.0, 500.0)), 0.2);
        assert_eq!(decimal_value(3_210, decimal(1000.0, 0.0)), 3.21);
        assert_eq!(decimal_value(1 << 30, Scale::Semicircles), 90.0);
    }
}
