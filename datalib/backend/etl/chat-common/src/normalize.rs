//! Small helpers a provider reaches for while building its
//! `NormalizedChat`: reading a stamp, naming an author after a role,
//! printing a JSON body the same way every time.

use serde_json::Value;

/// Unix millis from an RFC 3339 stamp with its offset; `None` for
/// anything else, which the caller records through
/// [`crate::own_stamp_ms`] before any fallback of its own.
pub fn iso_to_ms(s: &str) -> Option<i64> {
    datalib_time::parse_strict(s)
        .ok()
        .map(|t| t.to_unix_millis())
}

/// `ASSISTANT` → `Assistant`: the first letter upper-case, the rest lower.
pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out: String = c.to_uppercase().collect();
            for rest in chars {
                out.extend(rest.to_lowercase());
            }
            out
        }
    }
}

/// `v` pretty-printed with every object's keys sorted, so it reads the
/// same whatever order upstream wrote them in.
pub fn json_pretty_sorted(v: &Value) -> String {
    serde_json::to_string_pretty(&canonicalize(v)).unwrap_or_default()
}

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(pairs.len());
            for (k, val) in pairs {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp that will not parse must come back `None`, never the
    /// epoch: the caller then inherits the previous item's stamp or
    /// leaves `created_at` null. Malformed mail `Date` headers were a
    /// live source of fake-1970 grid rows.
    #[test]
    fn iso_to_ms_refuses_to_invent_a_timestamp() {
        assert_eq!(
            iso_to_ms("2026-04-14T09:15:00-07:00"),
            Some(1_776_183_300_000)
        );
        for bad in [
            "",
            "not a date",
            "2026-04-14",
            // A `Date` header that never made it through RFC 3339.
            "Tue, 14 Apr 2026 09:15:00 -0700",
            // Naive — no offset — which we refuse rather than assume.
            "2026-04-14T09:15:00",
        ] {
            assert_eq!(
                iso_to_ms(bad),
                None,
                "iso_to_ms({bad:?}) fabricated a stamp"
            );
        }
    }

    #[test]
    fn capitalize_lowers_the_rest() {
        assert_eq!(capitalize("ASSISTANT"), "Assistant");
        assert_eq!(capitalize("tool"), "Tool");
        assert_eq!(capitalize(""), "");
    }
}
