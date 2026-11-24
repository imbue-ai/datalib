//! `GET /children/{parent}`: a container's children in order, a page each
//! way at a time around an anchor
//! ([`datalib_unified_grid_and_search_index::children`]). The parent is a
//! uuid, or `source:<id>` for what a source holds at its top; `places=true`
//! lists only the places, as a library's tree does.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine as _;
use datalib_unified_grid_and_search_index::children::{
    Anchor, Ask, Child, ChildrenFailure, ChildrenPage, Key, Scope,
};
use serde::{Deserialize, Serialize};

use super::Index;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    /// An item's uuid, a moment (an RFC 3339 stamp), or `now`.
    at: Option<String>,
    after: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
    /// Only the places a person browses, each saying whether more lie
    /// below it.
    #[serde(default)]
    places: bool,
}

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Serialize)]
pub struct Response {
    children: Vec<Entry>,
    /// Where the page before this one starts, or null when this one is the
    /// first; `after` likewise.
    before: Option<String>,
    after: Option<String>,
    /// The commit the page was read at.
    at: Option<String>,
}

#[derive(Debug, Serialize)]
struct Entry {
    #[serde(flatten)]
    child: Child,
    /// Where opening it goes; null for a container with no page.
    link: Option<String>,
}

/// An error is `{"error": …}`, as the gateway's own are.
type Refusal = (StatusCode, Json<serde_json::Value>);

pub async fn handler(
    State(s): State<Index>,
    Path(parent): Path<String>,
    Query(p): Query<Params>,
) -> Result<Json<Response>, Refusal> {
    let refuse = |status, why: String| (status, Json(serde_json::json!({ "error": why })));
    let (ask, limit) =
        ask_of(&p, &super::now_utc()).map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    let scope = if p.places {
        Scope::places(&parent)
    } else {
        Scope::all(&parent)
    };
    match s.repo.children(&scope, &ask, limit).await {
        Ok(page) => Ok(Json(response(page))),
        Err(ChildrenFailure::Refused(why)) => Err(refuse(StatusCode::NOT_FOUND, why.to_string())),
        Err(ChildrenFailure::Repo(e)) => Err(refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list the children of {parent}: {e}"),
        )),
    }
}

#[derive(Debug, Serialize)]
pub struct SourcesWithPlaces {
    sources: Vec<String>,
}

/// `GET /sources_with_places`: which sources a library's tree opens onto
/// places, in one read.
pub async fn sources_with_places_handler(
    State(s): State<Index>,
) -> Result<Json<SourcesWithPlaces>, Refusal> {
    match s.repo.sources_with_places().await {
        Ok(sources) => Ok(Json(SourcesWithPlaces { sources })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// What `p` asks for, with `now` standing for `at=now`.
fn ask_of(p: &Params, now: &str) -> Result<(Ask, usize), String> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("limit is {limit}; it takes 1 to {MAX_LIMIT}"));
    }
    let ask = match (&p.at, &p.after, &p.before) {
        (None, None, None) => Ask::Around(Anchor::Start),
        (Some(at), None, None) => Ask::Around(anchor(at, now)),
        (None, Some(after), None) => Ask::After(key_of(after)?),
        (None, None, Some(before)) => Ask::Before(key_of(before)?),
        _ => return Err("give one of at, after and before".to_string()),
    };
    Ok((ask, limit))
}

/// A moment is compared with sort keys, so it is spelled as one: UTC,
/// to the microsecond.
fn anchor(at: &str, now: &str) -> Anchor {
    if at == "now" {
        return Anchor::Moment(now.to_string());
    }
    match datalib_time::split_record_stamp(at) {
        Some((utc, _offset)) => Anchor::Moment(utc),
        None => Anchor::Item(at.to_string()),
    }
}

/// A cursor is the edge row's key, opaque to whoever holds it.
fn cursor(key: &Key) -> String {
    let json = serde_json::to_vec(&(&key.sort_key, &key.uuid)).expect("two strings serialize");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn key_of(cursor: &str) -> Result<Key, String> {
    let refused = || format!("{cursor:?} is not a cursor this route gave");
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| refused())?;
    let (sort_key, uuid): (String, String) =
        serde_json::from_slice(&json).map_err(|_| refused())?;
    Ok(Key { sort_key, uuid })
}

fn response(page: ChildrenPage) -> Response {
    Response {
        children: page
            .children
            .into_iter()
            .map(|child| Entry {
                link: child
                    .markdown_uuid
                    .as_deref()
                    .map(|md| datalib_uri::Name::doc(md, Some(&child.uuid)).write()),
                child,
            })
            .collect(),
        before: page.before.as_ref().map(cursor),
        after: page.after.as_ref().map(cursor),
        at: page.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2364-02-11T09:00:00.000000Z";

    fn ask(at: Option<&str>, after: Option<&str>, before: Option<&str>) -> Result<Ask, String> {
        let p = Params {
            at: at.map(String::from),
            after: after.map(String::from),
            before: before.map(String::from),
            limit: None,
            places: false,
        };
        ask_of(&p, NOW).map(|(ask, _)| ask)
    }

    /// `at` is `now`, a stamp in any offset (spelled as a sort key is), or
    /// else an item; nothing at all is the start.
    #[test]
    fn at_reads_as_now_a_moment_or_an_item() {
        assert_eq!(
            ask(Some("now"), None, None),
            Ok(Ask::Around(Anchor::Moment(NOW.into())))
        );
        assert_eq!(
            ask(Some("2364-02-11T10:30:00+01:00"), None, None),
            Ok(Ask::Around(Anchor::Moment(
                "2364-02-11T09:30:00.000000Z".into()
            )))
        );
        assert_eq!(
            ask(Some("0b6e-thread"), None, None),
            Ok(Ask::Around(Anchor::Item("0b6e-thread".into())))
        );
        assert_eq!(ask(None, None, None), Ok(Ask::Around(Anchor::Start)));
    }

    /// A cursor goes back as the key it was made from, whatever the key
    /// holds; anything else, or two directions at once, is refused.
    #[test]
    fn a_cursor_round_trips_and_nothing_else_is_one() {
        let key = Key::new("2364-02-11T09:00:00.000000Z", "m/1?&=");
        let c = cursor(&key);
        assert!(
            c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)),
            "{c}"
        );
        assert_eq!(ask(None, Some(&c), None), Ok(Ask::After(key.clone())));
        assert_eq!(ask(None, None, Some(&c)), Ok(Ask::Before(key)));
        assert!(ask(None, Some("40"), None)
            .unwrap_err()
            .contains("not a cursor"));
        assert!(ask(Some("now"), Some(&c), None).is_err());
    }

    #[test]
    fn a_limit_out_of_range_is_refused() {
        let p = Params {
            limit: Some(0),
            ..Default::default()
        };
        assert!(ask_of(&p, NOW).is_err());
        let p = Params {
            limit: Some(MAX_LIMIT + 1),
            ..Default::default()
        };
        assert!(ask_of(&p, NOW).is_err());
    }
}
