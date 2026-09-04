//! A move-aware diff of two `fsindex` directory scans.
//!
//! `fsindex` records one row per file and directory — path, kind, size,
//! blake3 — into a doltlite store. This crate turns two such scans into
//! a single self-contained HTML page: the two trees side by side, with
//! **moves reported as moves** rather than as a delete plus an
//! unrelated create.
//!
//! That falls out of infrastructure the provider already has. A
//! directory's blake3 covers a canonical encoding of its children, so a
//! directory's digest covers its whole subtree; move it and the digest
//! is unchanged, and the prolly diff reports the same digest as a
//! `removed` row and an `added` row at two different paths. Pairing
//! those is the whole trick. See the crate's `README.md`.
//!
//! The code has a deliberate seam:
//!
//! ```text
//! doltlite ──▶ Inputs ──▶ analyze() ──▶ DiffResult ──▶ HTML
//!  (store)     (model)     (analyze)     (model)    └─▶ JSON
//! ```
//!
//! [`store`] is the only module that talks to a database, [`analyze`]
//! is pure, and both the page and `--json` are projections of
//! [`model::DiffResult`]. Everything worth testing is asserted against
//! the result with no doltlite and no browser in the way.

pub mod analyze;
pub mod model;
pub mod render;
pub mod store;

pub use analyze::analyze;
pub use model::{DiffResult, Inputs, Side, SideInput, Status};

/// Parse a byte threshold, accepting `4096`, `64K`, `1M`, `2G`.
pub fn parse_size(text: &str) -> anyhow::Result<i64> {
    let raw = text.trim().to_ascii_uppercase();
    let raw = raw.strip_suffix('B').unwrap_or(&raw);
    if raw.is_empty() {
        anyhow::bail!("empty size");
    }
    let (number, scale) = match raw.chars().last() {
        Some('K') => (&raw[..raw.len() - 1], 1024_f64),
        Some('M') => (&raw[..raw.len() - 1], 1024_f64 * 1024.0),
        Some('G') => (&raw[..raw.len() - 1], 1024_f64.powi(3)),
        Some('T') => (&raw[..raw.len() - 1], 1024_f64.powi(4)),
        _ => (raw, 1.0),
    };
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("not a byte size: {text:?}"))?;
    if value < 0.0 {
        anyhow::bail!("negative size: {text:?}");
    }
    Ok((value * scale) as i64)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn plain_and_suffixed_sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64K").unwrap(), 65536);
        assert_eq!(parse_size("1M").unwrap(), 1_048_576);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024_i64.pow(3));
        assert_eq!(parse_size("1.5M").unwrap(), 1_572_864);
        assert_eq!(parse_size("0").unwrap(), 0);
    }

    #[test]
    fn case_and_a_trailing_b_are_tolerated() {
        assert_eq!(parse_size("64k").unwrap(), 65536);
        assert_eq!(parse_size("64KB").unwrap(), 65536);
    }

    #[test]
    fn nonsense_is_rejected() {
        for bad in ["", "   ", "many", "12X", "-5"] {
            assert!(parse_size(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
