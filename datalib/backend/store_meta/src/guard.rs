//! The downgrade guard: a build never writes a store a newer line of
//! datalib wrote. An older build's reconcile would drop the columns it
//! does not know, and take the rows with them (`doltlite_raw`'s
//! drop-and-recreate), so the open is refused before anything touches
//! the file. docs/dev/plans/completed/schema_migrations.md §3.4.
//!
//! "Newer" is by `major.minor`: a patch release never changes a store's
//! shape (`docs/dev/release_steps.md`), so a store written by 0.36.3
//! opens under 0.36.1, and one written by 0.37.0 does not.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::Meta;

/// A store this build must not write: the line that wrote it is newer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewerBuild {
    pub store: PathBuf,
    /// `datalib_version` as the store records it.
    pub wrote: String,
    /// This build's version.
    pub running: String,
}

impl std::fmt::Display for NewerBuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} was written by datalib {}, and this is datalib {}: an older build \
             cannot open a store a newer one wrote without losing what the newer \
             one knew. Run datalib {} or later against this data root, or point \
             this one at another root.",
            self.store.display(),
            self.wrote,
            self.running,
            self.wrote
        )
    }
}

impl std::error::Error for NewerBuild {}

/// `major.minor` of a version string, ignoring the patch and any
/// suffix; `None` for a string that is not one.
fn line(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Whether `wrote` is a newer `major.minor` than `running`. A version
/// this cannot read is not newer: the guard refuses on evidence, and a
/// store that names its writer strangely is somebody else's problem.
pub fn written_by_a_newer_line(wrote: &str, running: &str) -> bool {
    match (line(wrote), line(running)) {
        (Some(w), Some(r)) => w > r,
        _ => false,
    }
}

/// The check every owner runs before it touches a store. `None` — a
/// store from before `_datalib_meta` existed — passes: it predates
/// every build that can ask.
pub fn refuse_if_newer(store: &Path, meta: Option<&Meta>) -> Result<(), NewerBuild> {
    let running = datalib_runtime::build_id::DATALIB_VERSION;
    match meta {
        Some(m) if written_by_a_newer_line(&m.datalib_version, running) => Err(NewerBuild {
            store: store.to_path_buf(),
            wrote: m.datalib_version.clone(),
            running: running.to_string(),
        }),
        _ => Ok(()),
    }
}

/// What `_datalib_meta` says in a store this process does not own,
/// read through a read-only connection that is closed before this
/// returns. `None` for a store with no table.
pub async fn read_at(store: &Path) -> Result<Option<Meta>> {
    let pool = datalib_pin::open_reader(store).await?;
    let meta = crate::read(&pool).await;
    pool.close().await;
    meta
}

/// Every store under a data root: each `*.doltlite_db` up to three
/// levels down (`system/`, `<group>/ingest/`, `<group>/render_markdown/`,
/// `unified_index/grid_index/`) and the run store. A walk rather than a
/// list of the layout's names, so a store this crate has not heard of
/// is inspected too.
pub fn stores_under(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < 3 {
                    walk(&path, depth + 1, out);
                }
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".doltlite_db") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 1, &mut out);
    let runs = datalib_runtime::layout::runs_db(root);
    if runs.is_file() {
        out.push(runs);
    }
    out.sort();
    out
}

/// The stores under a data root that this build must not write, in path
/// order. A store that cannot be read is skipped with a warning: the
/// guard refuses on evidence, and whoever owns that store will fail on
/// it loudly enough by itself.
pub async fn inspect_root(root: &Path) -> Vec<NewerBuild> {
    let mut out = Vec::new();
    for store in stores_under(root) {
        let meta = match read_at(&store).await {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(
                    store = %store.display(),
                    error = %format!("{e:#}"),
                    "downgrade guard: could not read this store's _datalib_meta; not counting it"
                );
                continue;
            }
        };
        if let Err(newer) = refuse_if_newer(&store, meta.as_ref()) {
            out.push(newer);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison is `major.minor` and nothing else, and an
    /// unreadable version never counts as newer.
    #[test]
    fn newer_is_by_major_and_minor_only() {
        assert!(written_by_a_newer_line("0.37.0", "0.36.9"));
        assert!(written_by_a_newer_line("1.0.0", "0.99.0"));
        assert!(
            !written_by_a_newer_line("0.36.3", "0.36.1"),
            "a patch is the same line"
        );
        assert!(!written_by_a_newer_line("0.36.1", "0.36.3"));
        assert!(!written_by_a_newer_line("0.35.0", "0.36.0"));
        assert!(!written_by_a_newer_line("unknown", "0.36.0"));
        assert!(!written_by_a_newer_line("0.36.0", "dev"));
        assert!(written_by_a_newer_line("0.37.0-rc1", "0.36.0"));
    }

    /// The walk finds every doltlite store the layout places, at every
    /// depth, and the run store, and nothing else.
    #[test]
    fn the_walk_finds_every_store_the_layout_places() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let files = [
            "system/feedback.doltlite_db",
            "system/runs/runs.sqlite",
            "system/feedback.doltlite_db.lock",
            "system/api-token",
            "slack/ingest/entities.doltlite_db",
            "slack/ingest/blobs.doltlite_db",
            "slack/render_markdown/indexed_markdown.doltlite_db",
            "slack/render_markdown/a/b/c.md",
            "unified_index/grid_index/db.doltlite_db",
            "unified_index/qmd_index/qmd/index.sqlite",
            "config.toml",
        ];
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
        }
        let found: Vec<String> = stores_under(root)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            found,
            vec![
                "slack/ingest/blobs.doltlite_db",
                "slack/ingest/entities.doltlite_db",
                "slack/render_markdown/indexed_markdown.doltlite_db",
                "system/feedback.doltlite_db",
                "system/runs/runs.sqlite",
                "unified_index/grid_index/db.doltlite_db",
            ]
        );
    }

    /// A root with one store written by a newer line is refused on that
    /// store and no other; a store with no meta at all passes.
    #[tokio::test]
    async fn a_root_is_refused_on_exactly_the_newer_stores() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let newer = root.join("a/ingest/entities.doltlite_db");
        let same = root.join("b/ingest/entities.doltlite_db");
        let bare = root.join("system/usage.doltlite_db");
        for p in [&newer, &same, &bare] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", p.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(opts)
                .await
                .unwrap();
            if *p != bare {
                crate::write(&pool, crate::StoreKind::Raw, "h", 0)
                    .await
                    .unwrap();
            }
            if *p == newer {
                sqlx::query(
                    "UPDATE _datalib_meta SET value = '99.0.0' WHERE key = 'datalib_version'",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            pool.close().await;
        }
        let refused = inspect_root(root).await;
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].store, newer);
        assert_eq!(refused[0].wrote, "99.0.0");
        assert_eq!(
            refused[0].running,
            datalib_runtime::build_id::DATALIB_VERSION
        );
        assert!(refused[0]
            .to_string()
            .contains("was written by datalib 99.0.0"));
    }
}
