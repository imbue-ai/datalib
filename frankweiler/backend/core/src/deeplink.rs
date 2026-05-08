//! Deep-link grammar shared between Tauri (`frankweiler://`) and the web hash.
//!
//! v0 implements parse/unparse for two routes:
//!     search?q=<text>&type=<message|chat>&before=<date>&after=<date>&grid=<b64>
//!     chat/<conversation_uuid>?msg=<message_uuid>&grid=<b64>
//!     prefs

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Search {
        params: BTreeMap<String, String>,
    },
    Chat {
        conversation_uuid: String,
        params: BTreeMap<String, String>,
    },
    Prefs,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("empty url")]
    Empty,
    #[error("unknown route: {0}")]
    UnknownRoute(String),
    #[error("missing chat uuid")]
    MissingChatUuid,
}

/// Parse a `frankweiler://` URL or a `#…` hash. Both forms share the same
/// grammar after the leading `frankweiler://` or `#` is stripped.
pub fn parse(url: &str) -> Result<Route, ParseError> {
    let s = url
        .strip_prefix("frankweiler://")
        .or_else(|| url.strip_prefix('#'))
        .or_else(|| url.strip_prefix('/'))
        .unwrap_or(url);
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let (path, query) = match s.split_once('?') {
        Some((p, q)) => (p, q),
        None => (s, ""),
    };
    let params = parse_query(query);
    let mut parts = path.splitn(2, '/');
    let head = parts.next().unwrap_or("");
    match head {
        "search" => Ok(Route::Search { params }),
        "prefs" => Ok(Route::Prefs),
        "chat" => {
            let uuid = parts.next().ok_or(ParseError::MissingChatUuid)?.to_string();
            if uuid.is_empty() {
                return Err(ParseError::MissingChatUuid);
            }
            Ok(Route::Chat {
                conversation_uuid: uuid,
                params,
            })
        }
        other => Err(ParseError::UnknownRoute(other.to_string())),
    }
}

/// Render a route to its hash form (without the leading `#`).
pub fn to_hash(route: &Route) -> String {
    match route {
        Route::Search { params } => with_query("search", params),
        Route::Chat {
            conversation_uuid,
            params,
        } => with_query(&format!("chat/{}", conversation_uuid), params),
        Route::Prefs => "prefs".to_string(),
    }
}

/// Render a route to its `frankweiler://` form.
pub fn to_deeplink(route: &Route) -> String {
    format!("frankweiler://{}", to_hash(route))
}

fn parse_query(q: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if q.is_empty() {
        return out;
    }
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn with_query(path: &str, params: &BTreeMap<String, String>) -> String {
    if params.is_empty() {
        return path.to_string();
    }
    let q = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{}", path, q)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe =
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':' | b',');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kvs: &[(&str, &str)]) -> BTreeMap<String, String> {
        kvs.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parses_search_hash_and_deeplink() {
        let want = Route::Search {
            params: p(&[("q", "treemap")]),
        };
        assert_eq!(parse("#search?q=treemap").unwrap(), want);
        assert_eq!(parse("frankweiler://search?q=treemap").unwrap(), want);
    }

    #[test]
    fn parses_chat() {
        assert_eq!(
            parse("frankweiler://chat/abc-123?msg=def-456").unwrap(),
            Route::Chat {
                conversation_uuid: "abc-123".into(),
                params: p(&[("msg", "def-456")]),
            },
        );
    }

    #[test]
    fn round_trip_search() {
        let r = Route::Search {
            params: p(&[("q", "hello world"), ("type", "message")]),
        };
        assert_eq!(parse(&format!("#{}", to_hash(&r))).unwrap(), r);
        assert_eq!(parse(&to_deeplink(&r)).unwrap(), r);
    }

    #[test]
    fn round_trip_chat_with_special_chars() {
        let r = Route::Chat {
            conversation_uuid: "abc-123".into(),
            params: p(&[("msg", "m/x?y=1"), ("grid", "AAAA==")]),
        };
        assert_eq!(parse(&to_deeplink(&r)).unwrap(), r);
    }

    #[test]
    fn rejects_unknown_route() {
        assert!(matches!(parse("#bogus"), Err(ParseError::UnknownRoute(_))));
    }

    #[test]
    fn rejects_chat_without_uuid() {
        assert!(matches!(parse("#chat/"), Err(ParseError::MissingChatUuid)));
    }
}
