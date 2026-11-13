//! The options that open one file, named by its literal path. Every open of a
//! store starts here, so a data root with an odd character in its path opens
//! the store that is there and no other.
//!
//! Two things would read a path as something else. sqlx's
//! `SqliteConnectOptions::from_str` reads a URL: it percent-decodes the path
//! and cuts it at the first `?`. And SQLite reads a name beginning `file:` as
//! a URI, `:memory:` as no file at all and an empty name as a temporary one,
//! which only a relative path can spell, so a relative path starts `./`.
//!
//! The one thing not opened here is a new plain SQLite file, which needs a URI
//! to say so: `datalib_runtime::plain_sqlite::uri`.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;

/// Read-write and refusing to create the file, as `SqliteConnectOptions::new`
/// is. A path that is not UTF-8 fails the connect.
pub fn file(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new().filename(literal(path))
}

/// `path` at a doltlite revision: a branch, a tag or a commit
/// (docs/dev/doltlite.md#opening-a-revision-by-path).
pub fn revision(path: &Path, rev: &str) -> SqliteConnectOptions {
    SqliteConnectOptions::new().filename(revision_name(path, rev))
}

/// The name SQLite opens for `path` at `rev`, for a caller of the C API.
pub fn revision_name(path: &Path, rev: &str) -> PathBuf {
    let mut name = literal(path).into_os_string();
    name.push("@");
    name.push(rev);
    name.into()
}

/// The name SQLite opens for `path` itself, for a caller of the C API.
pub fn literal(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(".").join(path)
    }
}
