//! A byte count a person can write: `5_000_000`, `"5 MB"`, `"512 KiB"`.
//!
//! Every `*_bytes` knob in a config deserializes through
//! [`deserialize_opt`], so it takes a plain integer or a string with a
//! unit. The grammar is `<number> [space] <unit>`: a decimal number with
//! optional `_` separators and an optional fraction; a unit that is `B`,
//! or one of `K M G T` with an optional `B` (decimal, ×1000) or `iB`
//! (binary, ×1024); case-insensitive. A bare number in a string is bytes.
//! The wizard's `bytes` control writes and reads the same grammar.

use serde::{Deserialize, Deserializer};

pub fn parse(text: &str) -> Result<u64, String> {
    let s = text.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number = number.replace('_', "");
    if number.is_empty() {
        return Err(format!("`{text}` has no number"));
    }
    let amount: f64 = number
        .parse()
        .map_err(|_| format!("`{text}`: `{number}` is not a number"))?;
    let multiplier = unit_multiplier(unit.trim()).ok_or_else(|| {
        format!(
            "`{text}`: unknown unit `{}` (want B, KB, MB, GB, TB, KiB, MiB, GiB or TiB)",
            unit.trim()
        )
    })?;
    let bytes = amount * multiplier as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("`{text}` is out of range"));
    }
    Ok(bytes.round() as u64)
}

fn unit_multiplier(unit: &str) -> Option<u64> {
    let u = unit.to_ascii_lowercase();
    if u.is_empty() || u == "b" {
        return Some(1);
    }
    let (prefix, rest) = u.split_at(1);
    let power = match prefix {
        "k" => 1,
        "m" => 2,
        "g" => 3,
        "t" => 4,
        _ => return None,
    };
    let base: u64 = match rest {
        "" | "b" => 1000,
        "ib" => 1024,
        _ => return None,
    };
    Some(base.pow(power))
}

/// `#[serde(default, deserialize_with = "byte_size::deserialize_opt")]`
/// on an `Option<u64>` field.
pub fn deserialize_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(u64),
        Text(String),
    }
    match Option::<Raw>::deserialize(d)? {
        None => Ok(None),
        Some(Raw::Int(n)) => Ok(Some(n)),
        Some(Raw::Text(s)) => parse(&s).map(Some).map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_spelling() {
        assert_eq!(parse("250"), Ok(250));
        assert_eq!(parse("5_000_000"), Ok(5_000_000));
        assert_eq!(parse("5 MB"), Ok(5_000_000));
        assert_eq!(parse("5MB"), Ok(5_000_000));
        assert_eq!(parse("5mb"), Ok(5_000_000));
        assert_eq!(parse("5 M"), Ok(5_000_000));
        assert_eq!(parse(" 1.5 GB "), Ok(1_500_000_000));
        assert_eq!(parse("512 MiB"), Ok(512 * 1024 * 1024));
        assert_eq!(parse("8 GiB"), Ok(8 * 1024 * 1024 * 1024));
        assert_eq!(parse("2 TB"), Ok(2_000_000_000_000));
        assert_eq!(parse("0 B"), Ok(0));
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert!(parse("").is_err());
        assert!(parse("MB").is_err());
        assert!(parse("5 XB").is_err());
        assert!(parse("5 MBs").is_err());
        assert!(parse("five MB").is_err());
        assert!(parse("1.2.3 KB").is_err());
    }

    #[test]
    fn deserializes_int_string_or_absent() {
        #[derive(Deserialize)]
        struct Knob {
            #[serde(default, deserialize_with = "deserialize_opt")]
            cap: Option<u64>,
        }
        let read = |json: &str| serde_json::from_str::<Knob>(json).map(|k| k.cap);
        assert_eq!(read(r#"{"cap": 5000000}"#).unwrap(), Some(5_000_000));
        assert_eq!(read(r#"{"cap": "5 MB"}"#).unwrap(), Some(5_000_000));
        assert_eq!(read(r#"{"cap": null}"#).unwrap(), None);
        assert_eq!(read(r#"{}"#).unwrap(), None);
        let err = read(r#"{"cap": "5 XB"}"#).unwrap_err().to_string();
        assert!(err.contains("unknown unit `XB`"), "{err}");
    }
}
