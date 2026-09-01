//! Give a GitLab payload a stable form before it is stored.
//!
//! # The problem
//!
//! GitLab embeds a cache-buster in avatar URLs:
//!
//! ```text
//! https://gitlab.com/uploads/-/system/user/avatar/20370006/avatar.png?v=1788242602
//! ```
//!
//! For at least some users that `?v=` is **the time of the fetch**, not
//! a property of the image. Measured against the live golden on
//! 2026-09-01: the value was `1788242602`, and the run that produced it
//! started at `1788242598` — four seconds earlier. So the stored payload
//! differs from itself on every fetch, `dolt_diff_merge_requests`
//! reports a change, and the manual-e2e golden churns on content that
//! never moved. One bake produced 31 changed lines from this alone.
//!
//! It is not uniform, which is what makes the field unusable rather than
//! merely noisy. Across two consecutive bakes:
//!
//! | avatar | `?v=` | |
//! |---|---|---|
//! | user 14374385 | `2026-05-15 00:00:00`, both times | stable — a real version |
//! | user 14376375 | `2026-08-31 00:14` → `2026-09-01 00:00` | rotates daily |
//! | user 20370006 | `2026-08-31 11:17` → `2026-09-01 06:03` | equals the fetch time |
//!
//! A field that is a content version for one row and a clock reading for
//! the next cannot be trusted as either.
//!
//! # Why strip the parameter rather than declare the field volatile
//!
//! The same reasoning `anthropic`'s `canonicalize_project_payload`
//! records for sorting `permissions` instead of dropping it: the
//! *contents* are content — an avatar actually changing is a change we
//! want to see — and it is only the cache-buster that carries no
//! information. Stripping `?v=` keeps the URL, so a different avatar
//! path still registers as a difference.
//!
//! There is also a mechanical reason `VOLATILE_PATHS` cannot do this
//! job. [`split_volatile`](datalib_etl::doltlite_raw::split_volatile)
//! takes fixed object-key paths from the payload root and skips any path
//! that would descend through a non-object. Of the 43 `avatar_url`
//! occurrences in one bake, 37 sit inside arrays —
//! `discussions[].payload.notes[].author.avatar_url`,
//! `merge_requests[].payload.reviewers[].avatar_url` — which no such
//! path can reach. Making it reach them would mean teaching a wildcard
//! segment to shared machinery that 130 targets depend on, to express
//! something a nine-line recursive walk says directly.
//!
//! # Scope
//!
//! Only a `v=<digits>` parameter, and only under a key named
//! `avatar_url`. Other query parameters survive, and a `v=` whose value
//! is not all digits is left alone — GitLab's cache-buster is a unix
//! timestamp, and anything else is more likely to be meaningful.

use serde_json::Value;

/// Recursively rewrite every `avatar_url` in `payload`, dropping the
/// `?v=<digits>` cache-buster.
///
/// Returns an owned copy; the input is untouched. Idempotent, so it is
/// safe to apply on a re-upsert of an already-stored payload.
pub fn canonicalize_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    strip_in_place(&mut out, false);
    out
}

/// `under_avatar_key` is true when the value we are looking at was
/// reached through a key named `avatar_url`, which is the only place a
/// string gets rewritten.
fn strip_in_place(v: &mut Value, under_avatar_key: bool) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                strip_in_place(child, k == "avatar_url");
            }
        }
        Value::Array(items) => {
            // An array under `avatar_url` is not a thing GitLab sends,
            // but propagating the flag costs nothing and means a future
            // shape change does not silently stop being canonicalized.
            for item in items.iter_mut() {
                strip_in_place(item, under_avatar_key);
            }
        }
        Value::String(s) if under_avatar_key => {
            if let Some(stripped) = strip_version_param(s) {
                *s = stripped;
            }
        }
        _ => {}
    }
}

/// Drop a `v=<digits>` parameter from `url`'s query string.
///
/// `None` when there is nothing to change, so the caller can skip the
/// write. Hand-rolled rather than routed through the `url` crate: this
/// is a textual edit on a value we must otherwise preserve byte for
/// byte, and a parse/serialize round-trip would also normalize
/// percent-encoding and default ports — rewriting URLs we were asked to
/// leave alone.
fn strip_version_param(url: &str) -> Option<String> {
    let (base, query) = url.split_once('?')?;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|param| {
            !param
                .strip_prefix("v=")
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    if kept.len() == query.split('&').count() {
        return None; // nothing matched
    }
    Some(if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AVATAR: &str = "https://gitlab.com/uploads/-/system/user/avatar/20370006/avatar.png";

    #[test]
    fn strips_the_cache_buster_and_keeps_the_url() {
        let p = json!({ "author": { "avatar_url": format!("{AVATAR}?v=1788242602") } });
        assert_eq!(
            canonicalize_payload(&p)["author"]["avatar_url"],
            json!(AVATAR)
        );
    }

    /// The point of the whole module: two fetches that differ only in
    /// the cache-buster must store the same bytes.
    #[test]
    fn two_fetches_of_one_avatar_canonicalize_the_same() {
        let a = json!({ "author": { "avatar_url": format!("{AVATAR}?v=1788175067") } });
        let b = json!({ "author": { "avatar_url": format!("{AVATAR}?v=1788242602") } });
        assert_ne!(a, b);
        assert_eq!(canonicalize_payload(&a), canonicalize_payload(&b));
    }

    /// …and the signal it must NOT erase: a different avatar is still a
    /// difference.
    #[test]
    fn a_different_avatar_still_differs() {
        let a = json!({ "author": { "avatar_url": format!("{AVATAR}?v=1") } });
        let b = json!({
            "author": { "avatar_url": "https://gitlab.com/uploads/-/system/user/avatar/999/avatar.png?v=1" }
        });
        assert_ne!(canonicalize_payload(&a), canonicalize_payload(&b));
    }

    /// 37 of the 43 real occurrences are array-nested, which is exactly
    /// what `split_volatile`'s object-key paths cannot reach.
    #[test]
    fn reaches_avatars_nested_inside_arrays() {
        let p = json!({
            "notes": [
                { "author": { "avatar_url": format!("{AVATAR}?v=111") } },
                { "resolved_by": { "avatar_url": format!("{AVATAR}?v=222") } },
            ],
            "reviewers": [{ "avatar_url": format!("{AVATAR}?v=333") }],
            "head_pipeline": { "user": { "avatar_url": format!("{AVATAR}?v=444") } },
        });
        let c = canonicalize_payload(&p);
        assert_eq!(c["notes"][0]["author"]["avatar_url"], json!(AVATAR));
        assert_eq!(c["notes"][1]["resolved_by"]["avatar_url"], json!(AVATAR));
        assert_eq!(c["reviewers"][0]["avatar_url"], json!(AVATAR));
        assert_eq!(c["head_pipeline"]["user"]["avatar_url"], json!(AVATAR));
    }

    #[test]
    fn other_query_parameters_survive() {
        let p = json!({ "avatar_url": format!("{AVATAR}?width=64&v=123&s=1") });
        assert_eq!(
            canonicalize_payload(&p)["avatar_url"],
            json!(format!("{AVATAR}?width=64&s=1"))
        );
    }

    #[test]
    fn a_non_numeric_v_is_left_alone() {
        // GitLab's cache-buster is a unix timestamp. Anything else is
        // more likely to mean something.
        let url = format!("{AVATAR}?v=abc");
        let p = json!({ "avatar_url": url.clone() });
        assert_eq!(canonicalize_payload(&p)["avatar_url"], json!(url));
    }

    #[test]
    fn only_avatar_url_keys_are_rewritten() {
        let other = "https://gitlab.com/x.png?v=999".to_string();
        let p = json!({ "web_url": other.clone(), "note_url": other.clone() });
        let c = canonicalize_payload(&p);
        assert_eq!(c["web_url"], json!(other));
        assert_eq!(c["note_url"], json!(other));
    }

    #[test]
    fn is_idempotent_and_leaves_clean_payloads_untouched() {
        let p = json!({ "author": { "avatar_url": AVATAR, "name": "Someone" }, "n": 1 });
        assert_eq!(canonicalize_payload(&p), p);
        let once = canonicalize_payload(&json!({ "avatar_url": format!("{AVATAR}?v=7") }));
        assert_eq!(canonicalize_payload(&once), once);
    }

    #[test]
    fn tolerates_nulls_and_odd_shapes() {
        let p = json!({ "author": { "avatar_url": Value::Null }, "reviewers": [], "x": 3 });
        assert_eq!(canonicalize_payload(&p), p);
    }
}
