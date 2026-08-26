//! JSON helpers shared by provider render stages.
//!
//! Renderers hash a provider's parsed rows to decide whether an
//! already-rendered `.md` file is still current, so the serialization
//! feeding that hash has to be stable across runs.
//!
//! # This is a guard, not a transformation
//!
//! With `serde_json`'s `preserve_order` feature **off** — the current
//! workspace configuration, and the crate default — `Value::Object` is
//! a `BTreeMap`, so it already iterates in sorted key order and
//! [`canonicalize`] is an identity function with respect to
//! serialization. It earns its place by making that independent of a
//! feature flag: turning `preserve_order` on (directly, or via any
//! dependency that enables it, since cargo features are unioned across
//! the graph) would switch `Value::Object` to an insertion-ordered
//! `IndexMap` and silently change every fingerprint in the tree.
//!
//! Four providers each carried a private copy of this function. They
//! now share one, and the invariant is asserted in one place.

use serde_json::Value;

/// Recursively sort every object's keys, leaving array order intact.
///
/// `to_string(&canonicalize(v))` is a stable fingerprint input for a
/// structurally-equal `Value` regardless of how it was built, and
/// regardless of whether `serde_json/preserve_order` is enabled. See
/// the module docs for why that second guarantee is the point.
pub fn canonicalize(v: &Value) -> Value {
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
    use serde_json::json;

    #[test]
    fn sorts_keys_recursively() {
        let v = json!({"b": 1, "a": {"d": 2, "c": 3}});
        assert_eq!(
            serde_json::to_string(&canonicalize(&v)).unwrap(),
            r#"{"a":{"c":3,"d":2},"b":1}"#
        );
    }

    #[test]
    fn preserves_array_order_but_sorts_within_elements() {
        let v = json!([{"b": 1, "a": 2}, {"d": 3, "c": 4}]);
        assert_eq!(
            serde_json::to_string(&canonicalize(&v)).unwrap(),
            r#"[{"a":2,"b":1},{"c":4,"d":3}]"#
        );
    }

    /// The property fingerprints depend on: two `Value`s built in
    /// different key orders serialize identically after canonicalizing.
    ///
    /// Note this passes trivially today — `preserve_order` is off, so
    /// the two parses are already equal before canonicalizing. It is
    /// here to fail if that ever changes, which is exactly when
    /// [`canonicalize`] starts doing real work.
    #[test]
    fn key_order_does_not_change_output() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":{"p":2,"q":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":{"q":3,"p":2},"x":1}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&canonicalize(&a)).unwrap(),
            serde_json::to_string(&canonicalize(&b)).unwrap()
        );
    }

    #[test]
    fn scalars_pass_through() {
        for s in ["null", "3", r#""s""#, "true"] {
            let v: Value = serde_json::from_str(s).unwrap();
            assert_eq!(canonicalize(&v), v);
        }
    }
}
