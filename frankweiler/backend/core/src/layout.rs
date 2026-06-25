//! Canonical `data_root` layout — the single source of truth for the
//! well-known directory names, shared by the writer (sync / config) and the
//! reader (the http server) so they can't drift.
//!
//! `data_root` holds one directory per source stanza (each user-named, owning
//! its `raw/` + `rendered_md/`) plus a single reserved `system/` directory for
//! everything that isn't a source: the UI-driving aggregate indices and the
//! server's runtime state.
//!
//! ```text
//! data_root/<stanza>/raw/…                          per-source extract
//! data_root/<stanza>/rendered_md/…                  per-source translate
//! data_root/system/backend_index/db.doltlite_db     grid_rows + markdowns index
//! data_root/system/qmd/index.sqlite                 qmd search index
//! data_root/system/media/…                          served attachments
//! data_root/system/state/job-logs/…                 sync job logs
//! ```
//!
//! Invariant: every top-level entry in `data_root` except `system/` is a
//! source stanza. `system` is therefore the one reserved stanza name.

use std::path::{Path, PathBuf};

/// The one reserved top-level directory: everything that isn't a source
/// stanza lives under here.
pub const SYSTEM_DIR: &str = "system";

/// Directory owned by the backend-index (grid_rows + markdowns) processor,
/// relative to `system/`.
pub const BACKEND_INDEX_DIR: &str = "backend_index";
/// The doltlite database file inside [`BACKEND_INDEX_DIR`].
pub const BACKEND_INDEX_DB: &str = "db.doltlite_db";

/// Directory owned by the qmd search-index processor, relative to `system/`.
pub const QMD_DIR: &str = "qmd";
/// Directory of server-served attachment bytes, relative to `system/`.
pub const MEDIA_DIR: &str = "media";
/// Directory of server runtime state (e.g. `job-logs/`), relative to
/// `system/`.
pub const STATE_DIR: &str = "state";

/// Stanza names a source may not take, because each would collide with a
/// reserved top-level directory on disk. With the `system/` split this is a
/// single name; the per-aggregate dirs live *inside* `system/`.
pub const RESERVED_STANZA_NAMES: &[&str] = &[SYSTEM_DIR];

/// `data_root/system`.
pub fn system_dir(data_root: &Path) -> PathBuf {
    data_root.join(SYSTEM_DIR)
}

/// `data_root/system/backend_index/db.doltlite_db` — the grid_rows/markdowns
/// index DB. The http server resolves this from `data_root` alone (it never
/// reads the config), so this helper is the contract between writer and reader.
pub fn backend_index_db(data_root: &Path) -> PathBuf {
    system_dir(data_root)
        .join(BACKEND_INDEX_DIR)
        .join(BACKEND_INDEX_DB)
}

/// `data_root/system/qmd` — the qmd index directory. qmd writes
/// `qmd/index.sqlite` under whatever it sees as `XDG_CACHE_HOME`, so the
/// cache home it runs with is [`system_dir`].
pub fn qmd_dir(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(QMD_DIR)
}

/// `data_root/system/media`.
pub fn media_dir(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(MEDIA_DIR)
}

/// `data_root/system/state`.
pub fn state_dir(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(STATE_DIR)
}
