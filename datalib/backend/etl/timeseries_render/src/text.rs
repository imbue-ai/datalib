//! Number and time formatting for the rendered page.

pub fn iso(ms: i64) -> Option<String> {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms).map(|t| t.to_rfc3339())
}

pub fn short(ms: i64) -> Option<String> {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.inner().format("%Y-%m-%d %H:%M").to_string())
}

pub fn short_ts(ms: i64) -> String {
    short(ms).unwrap_or_else(|| ms.to_string())
}

/// Median inter-sample gap, in ms. Median rather than mean because a
/// single multi-day outage would otherwise swamp a sensor that reports
/// every few minutes.
pub fn median_gap(ts: &[i64]) -> Option<i64> {
    if ts.len() < 2 {
        return None;
    }
    let mut gaps: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2])
}

pub fn human_gap(ms: i64) -> String {
    let s = ms as f64 / 1000.0;
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.1}m", s / 60.0)
    } else if s < 129_600.0 {
        format!("{:.1}h", s / 3600.0)
    } else {
        format!("{:.1}d", s / 86_400.0)
    }
}

pub fn thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

pub fn pretty_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

pub fn yaml_safe(s: &str) -> String {
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(149_564), "149,564");
        assert_eq!(thousands(-1_234_567), "-1,234,567");
    }

    #[test]
    fn median_gap_ignores_a_single_long_outage() {
        // Five 60s gaps and one 10-day gap: the median must stay 60s.
        let mut ts = vec![0i64];
        for _ in 0..5 {
            ts.push(ts.last().unwrap() + 60_000);
        }
        ts.push(ts.last().unwrap() + 864_000_000);
        assert_eq!(median_gap(&ts), Some(60_000));
        assert_eq!(median_gap(&[1]), None);
    }

    #[test]
    fn human_gap_picks_a_readable_unit() {
        assert_eq!(human_gap(30_000), "30s");
        assert_eq!(human_gap(300_000), "5.0m");
        assert_eq!(human_gap(7_200_000), "2.0h");
        assert_eq!(human_gap(432_000_000), "5.0d");
    }

    #[test]
    fn iso_stamps_carry_an_explicit_offset() {
        // AGENTS.md: an epoch-derived timestamp renders as UTC with an
        // explicit `+00:00`, never a bare `Z`-less or offset-less form.
        let s = iso(1_781_481_609_000).unwrap();
        assert!(s.starts_with("2026-"), "{s}");
        assert!(s.ends_with("+00:00"), "{s}");
    }
}
