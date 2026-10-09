//! `GET`/`PUT /api/ui/state/{name}`: JSON documents the UI keeps in the
//! library, so what a person arranged survives a restart and follows the
//! library to another machine. The browser's own storage cannot: the
//! desktop app's server takes a new port each launch, and each port is a
//! new origin with empty storage. The server never reads inside a
//! document; the UI owns its shape.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use std::path::{Path as FsPath, PathBuf};

use crate::AppState;

/// Far more than a layout tree; small enough that a runaway page cannot
/// fill the disk through here. Below axum's own 2 MiB body limit, so
/// this check is the one that answers.
pub const MAX_BYTES: usize = 1 << 20;

/// A name is a short word: `layout`, `composites`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

pub fn state_dir(root: &FsPath) -> PathBuf {
    root.join("system").join("ui-state")
}

pub async fn get_state(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !valid_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = state_dir(&s.root).join(format!("{name}.json"));
    match std::fs::read(&path) {
        Ok(bytes) => Ok(([(header::CONTENT_TYPE, "application/json")], bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("ui state: read {}: {e}", path.display());
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn put_state(
    State(s): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> StatusCode {
    if !valid_name(&name) {
        return StatusCode::BAD_REQUEST;
    }
    if body.len() > MAX_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return StatusCode::BAD_REQUEST;
    }
    let dir = state_dir(&s.root);
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| datalib_runtime::atomic::write(&dir.join(format!("{name}.json")), &body));
    match written {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("ui state: write {name}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn names_are_short_plain_words() {
        assert!(valid_name("layout"));
        assert!(valid_name("saved-composites_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("../config"));
        assert!(!valid_name("Layout"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}
