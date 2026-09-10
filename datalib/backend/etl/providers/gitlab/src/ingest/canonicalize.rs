//! Give a GitLab payload a stable form before it is stored.

use serde_json::Value;

pub fn canonicalize_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    strip_in_place(&mut out, false);
    out
}

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
