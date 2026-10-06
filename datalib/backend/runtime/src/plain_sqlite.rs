//! The name that makes doltlite create a stock SQLite file rather than one
//! in its own prolly-tree format.
//!
//! doltlite reads one URI parameter, `doltlite_engine`, and only when it
//! creates a new file: once the file has content its format decides. Pass
//! the result to sqlx as `SqliteConnectOptions::filename`, never through
//! `from_str` — sqlx's URL parser rejects a query parameter it does not
//! know, while `filename` reaches `sqlite3_open_v2` verbatim. That holds
//! only while sqlx adds no URI parameters of its own, so leave `immutable`
//! and `vfs` unset.

use std::path::Path;

pub fn uri(path: &Path) -> String {
    // SQLite percent-decodes a URI's path, so anything that would
    // terminate it or be decoded away has to be escaped. Spaces are fine
    // and are left alone — data roots have them.
    let escaped = path
        .display()
        .to_string()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{escaped}?doltlite_engine=sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_would_end_or_decode_the_uri_is_escaped() {
        assert_eq!(
            uri(Path::new("/data root/a%b?c#d.sqlite")),
            "file:/data root/a%25b%3fc%23d.sqlite?doltlite_engine=sqlite"
        );
    }
}
