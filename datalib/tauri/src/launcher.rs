//! The launcher's decisions, with no Tauri in them.
//!
//! Opening the app with no data root used to raise a native folder
//! picker immediately: a "choose a folder" sheet with no window behind
//! it and no explanation of what the folder was for. This module holds
//! everything the welcome screen that replaced it needs to decide —
//! which roots the user has opened before, whether a directory is a
//! data library at all, and where a brand-new one should go — so that
//! `main.rs` is left with window and IPC plumbing only.
//!
//! **Deliberately free of `tauri` and of every other dependency but
//! `serde_json`**, because this file is compiled twice: as a module of
//! the shell (cargo, the shipping build) and as its own crate by
//! `//datalib/tauri:launcher_test`. That second compile is the only
//! way any of this reaches `bazelisk test //...` — the shell crate is
//! a standalone cargo workspace that Bazel does not build (see
//! `Cargo.toml`). Keep it dependency-free and keep the tests here.

use std::path::{Path, PathBuf};

/// How many recent roots the welcome screen remembers. Enough to cover
/// a real rotation (work root, personal root, a scratch root or two)
/// and short enough to stay a list rather than a history.
pub const MAX_RECENTS: usize = 8;

/// The recents file: `<home>/.datalib/recent-roots.json`.
///
/// `~/.datalib/` is already the app's per-user directory — it is where
/// `bin/` lives, the drop spot for user-provided step and applet
/// programs (see `datalib_http::user_bin_dir`). One directory, not two.
pub fn recents_file(home: &Path) -> PathBuf {
    home.join(".datalib").join("recent-roots.json")
}

/// The remembered roots, newest first.
///
/// Entries that are no longer data roots are dropped rather than shown:
/// a folder the user deleted, renamed, or moved off an unmounted
/// volume would otherwise sit in the list as a button that fails. This
/// filters on read only — the file is rewritten by [`record_recent`],
/// so an unplugged drive's root comes back when it is plugged back in.
pub fn load_recents(file: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    parse_recents(&text)
        .into_iter()
        .filter(|p| is_data_root(p))
        .take(MAX_RECENTS)
        .collect()
}

/// Move `root` to the front of the remembered list and write it back.
///
/// Unlike [`load_recents`] this keeps entries whose directory is
/// currently missing: the list on disk is a memory of what the user
/// opened, and an unmounted volume is not a reason to forget it.
pub fn record_recent(file: &Path, root: &Path) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let mut roots = vec![root.to_path_buf()];
    for p in parse_recents(&existing) {
        if p != root && roots.len() < MAX_RECENTS {
            roots.push(p);
        }
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json: Vec<String> = roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let text = serde_json::to_string_pretty(&json).map_err(std::io::Error::other)?;
    std::fs::write(file, text)
}

/// Decode the file's contents. A corrupt or hand-mangled file reads as
/// an empty list — the recents are a convenience, and refusing to
/// launch over them would be worse than forgetting them.
fn parse_recents(text: &str) -> Vec<PathBuf> {
    serde_json::from_str::<Vec<String>>(text)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Whether `dir` is a datalib data root.
///
/// A root is identified by its config — `config.toml`, or the pre-TOML
/// `config.yaml` that `datalib-migrate-config` converts (the app has
/// something to say about those too, so hiding them from the recents
/// list would be the wrong kind of tidy). `system/` is accepted as
/// well: a root whose config was deleted still holds the user's
/// feedback and job stores, and is not an empty folder.
pub fn is_data_root(dir: &Path) -> bool {
    dir.is_dir()
        && (dir.join("config.toml").is_file()
            || dir.join("config.yaml").is_file()
            || dir.join("system").is_dir())
}

/// Where "create a new data library" should put one, inside
/// `documents` (the platform's Documents directory, which the caller
/// resolves — it is localized and relocatable, so it is not
/// `<home>/Documents` everywhere).
///
/// Documents because that is where a user looks for their own files,
/// and because the root is exactly that — their data, not application
/// state. If the name is taken by something that is not already a data
/// library, the next free `Datalib 2`, `Datalib 3`, … is used rather
/// than merging into a stranger's directory. An existing *data
/// library* at the default name is returned as-is: opening it is what
/// the user meant.
pub fn default_new_root(documents: &Path) -> PathBuf {
    let first = documents.join("Datalib");
    if !first.exists() || is_data_root(&first) {
        return first;
    }
    // Bounded so a pathological directory can't spin: past a few
    // collisions the name is not the problem, and the user can pick a
    // folder by hand.
    for n in 2..100 {
        let candidate = documents.join(format!("Datalib {n}"));
        if !candidate.exists() || is_data_root(&candidate) {
            return candidate;
        }
    }
    first
}

/// What to call a root in a list: its own folder name, falling back to
/// the whole path for a root at a filesystem's top level.
pub fn display_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that reads as a data root, the cheapest way.
    fn make_root(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.toml"), "steps = []\n").unwrap();
    }

    #[test]
    fn recents_file_lives_under_the_app_dir() {
        let f = recents_file(Path::new("/home/x"));
        assert_eq!(f, PathBuf::from("/home/x/.datalib/recent-roots.json"));
    }

    #[test]
    fn a_missing_recents_file_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_recents(&tmp.path().join("nope.json")).is_empty());
    }

    /// The recents file is not a format anyone should have to repair by
    /// hand; garbage in it must not stop the app from launching.
    #[test]
    fn a_corrupt_recents_file_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        std::fs::write(&f, "{not json at all").unwrap();
        assert!(load_recents(&f).is_empty());
    }

    #[test]
    fn recording_puts_the_newest_first_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join(".datalib/recent-roots.json");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        make_root(&a);
        make_root(&b);

        record_recent(&f, &a).unwrap();
        record_recent(&f, &b).unwrap();
        assert_eq!(load_recents(&f), vec![b.clone(), a.clone()]);

        // Re-opening `a` moves it up; it does not appear twice.
        record_recent(&f, &a).unwrap();
        assert_eq!(load_recents(&f), vec![a, b]);
    }

    #[test]
    fn recents_are_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        for i in 0..(MAX_RECENTS + 4) {
            let d = tmp.path().join(format!("root{i}"));
            make_root(&d);
            record_recent(&f, &d).unwrap();
        }
        let got = load_recents(&f);
        assert_eq!(got.len(), MAX_RECENTS);
        assert_eq!(
            got[0],
            tmp.path().join(format!("root{}", MAX_RECENTS + 3)),
            "newest first"
        );
    }

    /// A recent whose folder is gone must not be offered: the button
    /// would start a backend against a directory that no longer exists.
    #[test]
    fn a_vanished_root_is_not_offered() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let gone = tmp.path().join("gone");
        let kept = tmp.path().join("kept");
        make_root(&gone);
        make_root(&kept);
        record_recent(&f, &gone).unwrap();
        record_recent(&f, &kept).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        assert_eq!(load_recents(&f), vec![kept]);
        // …but the file still remembers it, so a remounted volume or a
        // restored folder comes back on its own.
        let raw = std::fs::read_to_string(&f).unwrap();
        assert!(raw.contains("gone"), "{raw}");
    }

    /// Paths are round-tripped, not reconstructed. A line-oriented
    /// format would split this one in half and silently lose it.
    #[test]
    fn an_awkward_path_survives_the_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let weird = tmp.path().join("two\nlines \"quoted\"");
        make_root(&weird);
        record_recent(&f, &weird).unwrap();
        assert_eq!(load_recents(&f), vec![weird]);
    }

    #[test]
    fn an_empty_folder_is_not_a_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_data_root(tmp.path()));
        assert!(!is_data_root(&tmp.path().join("missing")));

        make_root(&tmp.path().join("with_toml"));
        assert!(is_data_root(&tmp.path().join("with_toml")));
    }

    /// A pre-TOML root is still a root: the app has a migration screen
    /// for it, which it can only show if the folder can be opened.
    #[test]
    fn a_pre_toml_root_still_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "sources: []\n").unwrap();
        assert!(is_data_root(&dir));
    }

    /// A root whose config was deleted still holds feedback and job
    /// stores under `system/`, so it is not an empty folder.
    #[test]
    fn a_config_less_root_with_stores_still_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("stores_only");
        std::fs::create_dir_all(dir.join("system")).unwrap();
        assert!(is_data_root(&dir));
    }

    #[test]
    fn a_new_library_is_named_datalib() {
        let tmp = tempfile::tempdir().unwrap();
        let documents = tmp.path().join("Documents");
        assert_eq!(default_new_root(&documents), documents.join("Datalib"));
    }

    #[test]
    fn a_root_is_listed_under_its_folder_name() {
        assert_eq!(display_name(Path::new("/a/b/Work Library")), "Work Library");
        assert_eq!(display_name(Path::new("/")), "/");
    }

    /// The default name already holding a library means "open it",
    /// not "make a second one".
    #[test]
    fn an_existing_library_at_the_default_name_is_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let documents = tmp.path().join("Documents");
        let dir = documents.join("Datalib");
        make_root(&dir);
        assert_eq!(default_new_root(&documents), dir);
    }

    /// …but a stranger's `Documents/Datalib` — someone's unrelated
    /// folder of notes — must not be adopted as a data root and
    /// written into.
    #[test]
    fn an_unrelated_folder_at_the_default_name_is_stepped_around() {
        let tmp = tempfile::tempdir().unwrap();
        let documents = tmp.path().join("Documents");
        let taken = documents.join("Datalib");
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("notes.txt"), "mine").unwrap();

        assert_eq!(default_new_root(&documents), documents.join("Datalib 2"));

        // And it steps around each collision in turn.
        let also_taken = documents.join("Datalib 2");
        std::fs::create_dir_all(&also_taken).unwrap();
        std::fs::write(also_taken.join("notes.txt"), "also mine").unwrap();
        assert_eq!(default_new_root(&documents), documents.join("Datalib 3"));
    }
}
