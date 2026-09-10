//! Canonical `data_root` layout — the single source of truth for the
//! well-known directory names, shared by the writer (sync / config) and the
//! reader (the http server) so they can't drift.

use std::path::{Path, PathBuf};

/// The one reserved top-level directory: everything that isn't a source
/// stanza lives under here.
pub const SYSTEM_DIR: &str = "system";

/// The top-level tree holding every search index. Owned end to end by
/// the `unified_index` applet and the two steps that write it; nothing
/// in `datalib-http` or `datalib-dag` reads what is under here.
pub const UNIFIED_INDEX_DIR: &str = "unified_index";
/// The `grid_index` step's tree (grid_rows + markdowns + edges), relative
/// to [`UNIFIED_INDEX_DIR`]. Named after the step's function, like every
/// tree a step writes.
pub const GRID_DIR: &str = "grid_index";
/// The doltlite database file inside [`GRID_DIR`].
pub const GRID_DB: &str = "db.doltlite_db";
/// The `qmd_index` step's tree, relative to [`UNIFIED_INDEX_DIR`]. qmd
/// itself lays out `qmd/index.sqlite` under it — see
/// `crate::qmd::qmd_cache_home`.
pub const QMD_DIR: &str = "qmd_index";

/// Directory of server-served attachment bytes, relative to `system/`.
pub const MEDIA_DIR: &str = "media";
/// Filed feedback, relative to `system/`. Its own file because it has a
/// different writer from every other store and, unlike the indexes, it
/// cannot be regenerated.
pub const FEEDBACK_DB: &str = "feedback.doltlite_db";
/// The sync job queue and its history, relative to `system/`. Separate
/// from [`FEEDBACK_DB`] so a job update and a feedback commit cannot
/// land in each other's dolt history.
pub const JOBS_DB: &str = "jobs.doltlite_db";
/// The bytes-on-disk timeseries, relative to `system/`. Its own file
/// for the reason every store here has one: doltlite's working set is
/// per file, so a sample landing between two job transitions would be
/// swept into whichever commit came next. Nothing commits this one at
/// all — the rows are the history.
pub const USAGE_DB: &str = "usage.doltlite_db";
/// The server's exclusive claim on this root, relative to `system/`.
/// Held with `flock(2)` for the life of the process; its contents are
/// advisory, naming the holder so a refused server can say where the
/// running one is.
pub const LOCK_FILE: &str = "lock";

// Stanza names a source may not take live with the code that enforces
// them: `datalib_dag::config::RESERVED_STANZA_NAMES`, checked by
// `validate_steps` on the path every entry point already takes. A
// constant here had no callers at all, so nothing stopped a source
// named `system` — and it predated `unified_index/` becoming a second
// reserved top-level directory. `datalib-dag` deliberately doesn't
// depend on this crate, so the policy lives where it is applied and
// the path constants stay here.

pub fn system_dir(data_root: &Path) -> PathBuf {
    data_root.join(SYSTEM_DIR)
}

pub fn unified_index_dir(data_root: &Path) -> PathBuf {
    data_root.join(UNIFIED_INDEX_DIR)
}

pub fn grid_index_dir(data_root: &Path) -> PathBuf {
    unified_index_dir(data_root).join(GRID_DIR)
}

/// `data_root/unified_index/grid_index/db.doltlite_db` — the
/// grid_rows/markdowns/edges index. Resolved from `data_root` alone by
/// both the step that writes it and the applet that reads it, so this
/// helper is the contract between them.
pub fn grid_index_db(data_root: &Path) -> PathBuf {
    grid_index_dir(data_root).join(GRID_DB)
}

pub fn qmd_dir(data_root: &Path) -> PathBuf {
    unified_index_dir(data_root).join(QMD_DIR)
}

pub fn media_dir(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(MEDIA_DIR)
}

pub fn feedback_db(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(FEEDBACK_DB)
}

pub fn jobs_db(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(JOBS_DB)
}

pub fn usage_db(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(USAGE_DB)
}

pub fn lock_file(data_root: &Path) -> PathBuf {
    system_dir(data_root).join(LOCK_FILE)
}

/// Body of the `CACHEDIR.TAG` files we drop into derived directories. The
/// first line is the spec-mandated magic that `restic`/`borg`/`tar
/// --exclude-caches` (and others) recognize; see <https://bford.info/cachedir/>.
/// The rest is a human hint. Only the per-group `ingest/` stores are
/// precious — everything tagged here is 100% derived and rebuilt from them
/// by re-running the pipeline (`datalib-dag`).
pub const CACHEDIR_TAG_BODY: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This directory holds derived, rebuildable data (not a backup source).\n\
    # datalib regenerates it from the sibling/per-group ingest/ stores by\n\
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
        let derived = td.path().join("render_markdown");

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
