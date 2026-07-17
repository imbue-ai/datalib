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

/// `data_root/system/backend_index` — the dir holding the grid_rows/markdowns
/// index DB (and its `CACHEDIR.TAG`).
pub fn backend_index_dir(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(BACKEND_INDEX_DIR)
}

/// `data_root/system/backend_index/db.doltlite_db` — the grid_rows/markdowns
/// index DB. The http server resolves this from `data_root` alone (it never
/// reads the config), so this helper is the contract between writer and reader.
pub fn backend_index_db(data_root: &Path) -> PathBuf {
    backend_index_dir(data_root).join(BACKEND_INDEX_DB)
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

/// Body of the `CACHEDIR.TAG` files we drop into derived directories. The
/// first line is the spec-mandated magic that `restic`/`borg`/`tar
/// --exclude-caches` (and others) recognize; see <https://bford.info/cachedir/>.
/// The rest is a human hint. Only the per-stanza `raw/` stores are precious —
/// everything tagged here is 100% derived and rebuilt from raw by
/// re-running the pipeline (`datalib-dag`).
pub const CACHEDIR_TAG_BODY: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This directory holds derived, rebuildable data (not a backup source).\n\
    # frankweiler regenerates it from the sibling/per-stanza raw/ stores by\n\
    # re-running the pipeline (datalib-dag). Safe for backups to skip.\n\
    # See https://bford.info/cachedir/\n";

/// Drop a `CACHEDIR.TAG` into `dir` (if `dir` exists and the tag is absent),
/// marking it and everything below as derived cache so `--exclude-caches`
/// backups skip it. Best-effort: a write failure is swallowed — the tag is a
/// backup hint, never load-bearing for the pipeline.
pub fn mark_derived_cache(dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    let tag = dir.join("CACHEDIR.TAG");
    if !tag.exists() {
        let _ = std::fs::write(&tag, CACHEDIR_TAG_BODY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cachedir_tag_body_has_spec_signature() {
        // The first line must be the exact magic or `--exclude-caches` tools
        // won't recognize it.
        assert!(CACHEDIR_TAG_BODY.starts_with("Signature: 8a477f597d28d172789f06886806bc55\n"));
    }

    #[test]
    fn mark_derived_cache_writes_tag_once_and_skips_missing() {
        let td = tempfile::tempdir().unwrap();
        let derived = td.path().join("rendered_md");

        // Missing dir: no-op, no panic, nothing created.
        mark_derived_cache(&derived);
        assert!(!derived.exists());

        std::fs::create_dir_all(&derived).unwrap();
        mark_derived_cache(&derived);
        let tag = derived.join("CACHEDIR.TAG");
        assert!(tag.is_file());
        assert!(std::fs::read_to_string(&tag)
            .unwrap()
            .starts_with("Signature: 8a477f597d28d172789f06886806bc55"));

        // Idempotent: a second call doesn't clobber a user-edited tag.
        std::fs::write(&tag, "custom").unwrap();
        mark_derived_cache(&derived);
        assert_eq!(std::fs::read_to_string(&tag).unwrap(), "custom");
    }
}
