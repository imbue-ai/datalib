//! The libraries screen's decisions, with no Tauri in them: which
//! libraries it lists, and where a new one goes.

use std::path::{Path, PathBuf};

/// How many recent roots the libraries screen remembers. Enough to cover
/// a real rotation (work root, personal root, a scratch root or two)
/// and short enough to stay a list rather than a history.
pub const MAX_RECENTS: usize = 8;

/// The name a person's first library gets, inside [`libraries_dir`].
pub const DEFAULT_NAME: &str = "Default";

pub fn recents_file(home: &Path) -> PathBuf {
    home.join(".datalib").join("recent-roots.json")
}

/// Where a library given by name lives: `Datalib` in `documents` (the
/// platform's Documents directory, which the caller resolves — it is
/// localized and relocatable, so it is not `<home>/Documents`
/// everywhere).
pub fn libraries_dir(documents: &Path) -> PathBuf {
    documents.join("Datalib")
}

pub fn record_recent(file: &Path, root: &Path) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let mut roots = vec![root.to_path_buf()];
    for p in parse_recents(&existing) {
        if p != root && roots.len() < MAX_RECENTS {
            roots.push(p);
        }
    }
    write_recents(file, &roots)
}

/// Whether the screen offers to forget `root`. A library in the Datalib
/// folder is listed because it is there, not because it was opened, so
/// it stays while it is there; one elsewhere is listed only from the
/// recent list, and so is one whose folder is gone.
pub fn forgettable(root: &Path, libraries_dir: &Path) -> bool {
    if root == libraries_dir && is_data_root(root) {
        return false;
    }
    root.parent() != Some(libraries_dir) || !is_data_root(root)
}

/// A config's top-level `data_root` defaults to the config's own folder,
/// which moves with it; one written out as the old folder would keep
/// pointing there. Drop that line, and only that line: any other value
/// was chosen on purpose.
fn drop_own_data_root(config: &Path, old_root: &Path) -> std::io::Result<()> {
    let Ok(text) = std::fs::read_to_string(config) else {
        return Ok(());
    };
    let old = old_root.to_string_lossy();
    let names_old = |line: &str| {
        let Some(value) = line.trim().strip_prefix("data_root") else {
            return false;
        };
        let value = value.trim_start().strip_prefix('=').map(str::trim);
        value
            .and_then(|v| v.strip_prefix('"')?.strip_suffix('"'))
            .is_some_and(|v| v.trim_end_matches('/') == old.trim_end_matches('/'))
    };
    // Top-level keys come before the first `[` table header.
    let mut top = true;
    let mut changed = false;
    let mut kept = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            top = false;
        }
        if top && names_old(line) {
            changed = true;
            continue;
        }
        kept.push(line);
    }
    if changed {
        std::fs::write(config, kept.concat())?;
    }
    Ok(())
}

// TODO(after 2026-11-01): remove the move of a library at the Datalib
// folder itself into `Datalib/Default` (`legacy_root`, `move_legacy`,
// `Target::InsideLegacy`, the launcher's `launcher_move_legacy` and the
// screen's banner), and the special cases for it in `libraries`,
// `suggested_name`, `classify` and `forgettable`. By then the libraries
// made before each got its own folder will have been moved.

/// The library made before libraries each got a folder inside the
/// Datalib folder: the Datalib folder itself. `None` once it is moved.
pub fn legacy_root(libraries_dir: &Path) -> Option<PathBuf> {
    is_data_root(libraries_dir).then(|| libraries_dir.to_path_buf())
}

/// Move the library at the Datalib folder into `Datalib/Default`, where
/// a first library goes now, and point the recent list at it. Every
/// entry is renamed into a staging folder that is then renamed to
/// `Default`, so a half-done move never looks like a library at either
/// place. The caller makes sure the library is not open.
pub fn move_legacy(recents_file: &Path, libraries_dir: &Path) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    if legacy_root(libraries_dir).is_none() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("{} is not a library", libraries_dir.display()),
        ));
    }
    let target = libraries_dir.join(DEFAULT_NAME);
    if target.exists() {
        return Err(Error::new(
            ErrorKind::AlreadyExists,
            format!("{} already exists", target.display()),
        ));
    }
    let staging = libraries_dir.join(".Default-moving");
    std::fs::create_dir(&staging)?;
    for entry in std::fs::read_dir(libraries_dir)? {
        let from = entry?.path();
        if from == staging {
            continue;
        }
        let name = from.file_name().expect("a directory entry has a name");
        std::fs::rename(&from, staging.join(name))?;
    }
    std::fs::rename(&staging, &target)?;
    drop_own_data_root(&target.join("config.toml"), libraries_dir)?;
    forget_recent(recents_file, libraries_dir)?;
    record_recent(recents_file, &target)?;
    Ok(target)
}

/// Take `root` off the recent list. Only the list changes: the library's
/// folder and everything in it stay where they are, and opening it again
/// lists it again.
pub fn forget_recent(file: &Path, root: &Path) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let roots: Vec<PathBuf> = parse_recents(&existing)
        .into_iter()
        .filter(|p| p != root)
        .collect();
    write_recents(file, &roots)
}

fn write_recents(file: &Path, roots: &[PathBuf]) -> std::io::Result<()> {
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

fn parse_recents(text: &str) -> Vec<PathBuf> {
    serde_json::from_str::<Vec<String>>(text)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

pub fn is_data_root(dir: &Path) -> bool {
    dir.is_dir() && (dir.join("config.toml").is_file() || dir.join("system").is_dir())
}

/// A library the screen lists. `found` is false for a recent one whose
/// folder is gone — a drive not plugged in, say — which stays listed so
/// it comes back on its own when the folder does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    pub path: PathBuf,
    pub found: bool,
}

/// The recent libraries, newest first, then any other library in
/// `libraries_dir` by name: one made there and never opened again is
/// still the person's. While the Datalib folder is itself a library
/// (see [`legacy_root`]) its subfolders are that library's own, so it
/// is listed instead of them.
pub fn libraries(recents_file: &Path, libraries_dir: &Path) -> Vec<Library> {
    let recents = parse_recents(&std::fs::read_to_string(recents_file).unwrap_or_default());
    let mut out: Vec<Library> = recents
        .into_iter()
        .take(MAX_RECENTS)
        .map(|path| Library {
            found: is_data_root(&path),
            path,
        })
        .collect();
    if let Some(legacy) = legacy_root(libraries_dir) {
        if !out.iter().any(|l| l.path == legacy) {
            out.push(Library {
                path: legacy,
                found: true,
            });
        }
        return out;
    }
    let mut beside: Vec<PathBuf> = std::fs::read_dir(libraries_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_data_root(p) && !out.iter().any(|l| &l.path == p))
        .collect();
    beside.sort();
    out.extend(beside.into_iter().map(|path| Library { path, found: true }));
    out
}

/// What the new-library field starts with: the default library's name
/// until there is one, then nothing.
pub fn suggested_name(libraries_dir: &Path) -> &'static str {
    if libraries_dir.join(DEFAULT_NAME).exists() || legacy_root(libraries_dir).is_some() {
        ""
    } else {
        DEFAULT_NAME
    }
}

/// The folder a new-library field names. A plain name is a library in
/// `libraries_dir`; `/…` and `~/…` are taken as they are. `None` for an
/// empty field.
pub fn resolve_new(input: &str, home: &Path, libraries_dir: &Path) -> Option<PathBuf> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if t == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = t.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    let p = Path::new(t);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(libraries_dir.join(t))
}

/// What is at the folder a new library would go in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Nothing there, or an empty folder: a library can go here.
    New,
    /// Already a library: open it instead of making a second one.
    Library,
    /// A folder with other things in it. Not written into: it is
    /// somebody's, and a library's stores would land among their files.
    Occupied,
    /// A file, not a folder.
    NotAFolder,
    /// Inside the Datalib folder while it is still one library: moving
    /// that library into `Default` comes first.
    InsideLegacy,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::New => "new",
            Target::Library => "library",
            Target::Occupied => "occupied",
            Target::NotAFolder => "not_a_folder",
            Target::InsideLegacy => "inside_legacy",
        }
    }
}

pub fn classify(dir: &Path, libraries_dir: &Path) -> Target {
    if legacy_root(libraries_dir).is_some() && dir.starts_with(libraries_dir) {
        return Target::InsideLegacy;
    }
    if is_data_root(dir) {
        return Target::Library;
    }
    if !dir.exists() {
        return Target::New;
    }
    if !dir.is_dir() {
        return Target::NotAFolder;
    }
    let empty = std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none());
    if empty {
        Target::New
    } else {
        Target::Occupied
    }
}

/// A path as the screen shows it: under the home directory, from `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// What `datalib-http` last wrote down about a library: its source
/// count, bytes on disk and when it last synced
/// (`http/src/manage/summary.rs`). `None` for a library that server
/// has not answered the manage rows for since that file existed.
pub fn summary(root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join("system").join("library-summary.json")).ok()?;
    serde_json::from_str(&text).ok()
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

    fn make_root(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.toml"), "steps = []\n").unwrap();
    }

    fn paths(libs: &[Library]) -> Vec<PathBuf> {
        libs.iter().map(|l| l.path.clone()).collect()
    }

    #[test]
    fn recents_file_lives_under_the_app_dir() {
        let f = recents_file(Path::new("/home/x"));
        assert_eq!(f, PathBuf::from("/home/x/.datalib/recent-roots.json"));
    }

    #[test]
    fn a_missing_recents_file_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let none = tmp.path().join("none");
        assert!(libraries(&tmp.path().join("nope.json"), &none).is_empty());
    }

    /// The recents file is not a format anyone should have to repair by
    /// hand; garbage in it must not stop the app from launching.
    #[test]
    fn a_corrupt_recents_file_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        std::fs::write(&f, "{not json at all").unwrap();
        assert!(libraries(&f, &tmp.path().join("none")).is_empty());
    }

    #[test]
    fn recording_puts_the_newest_first_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join(".datalib/recent-roots.json");
        let none = tmp.path().join("none");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        make_root(&a);
        make_root(&b);

        record_recent(&f, &a).unwrap();
        record_recent(&f, &b).unwrap();
        assert_eq!(paths(&libraries(&f, &none)), vec![b.clone(), a.clone()]);

        // Re-opening `a` moves it up; it does not appear twice.
        record_recent(&f, &a).unwrap();
        assert_eq!(paths(&libraries(&f, &none)), vec![a, b]);
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
        let got = libraries(&f, &tmp.path().join("none"));
        assert_eq!(got.len(), MAX_RECENTS);
        assert_eq!(
            got[0].path,
            tmp.path().join(format!("root{}", MAX_RECENTS + 3)),
            "newest first"
        );
    }

    /// A recent whose folder is gone stays listed as not found, so a
    /// remounted volume or a restored folder comes back on its own, and
    /// forgetting it is what takes it off.
    #[test]
    fn a_vanished_root_is_listed_as_not_found_until_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let none = tmp.path().join("none");
        let gone = tmp.path().join("gone");
        let kept = tmp.path().join("kept");
        make_root(&gone);
        make_root(&kept);
        record_recent(&f, &gone).unwrap();
        record_recent(&f, &kept).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        assert_eq!(
            libraries(&f, &none),
            vec![
                Library {
                    path: kept.clone(),
                    found: true
                },
                Library {
                    path: gone.clone(),
                    found: false
                },
            ]
        );
        forget_recent(&f, &gone).unwrap();
        assert_eq!(paths(&libraries(&f, &none)), vec![kept]);
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
        assert_eq!(paths(&libraries(&f, &tmp.path().join("none"))), vec![weird]);
    }

    /// A library made in the Datalib folder and never opened again is
    /// listed after the recent ones; one that is both is listed once.
    #[test]
    fn libraries_in_the_datalib_folder_are_listed_after_the_recent_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("Documents"));
        let work = dir.join("Work");
        let default = dir.join(DEFAULT_NAME);
        let elsewhere = tmp.path().join("elsewhere");
        make_root(&work);
        make_root(&default);
        make_root(&elsewhere);
        std::fs::create_dir_all(dir.join("not a library")).unwrap();
        record_recent(&f, &work).unwrap();
        record_recent(&f, &elsewhere).unwrap();

        assert_eq!(paths(&libraries(&f, &dir)), vec![elsewhere, work, default]);
    }

    /// The Datalib folder's libraries are always listed, so forgetting
    /// one would not stick; one whose folder is gone can be, or it would
    /// sit on the list for good.
    #[test]
    fn only_a_library_outside_the_datalib_folder_or_gone_can_be_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = libraries_dir(&tmp.path().join("Documents"));
        let default = dir.join(DEFAULT_NAME);
        let nested = dir.join("nested").join("lib");
        let elsewhere = tmp.path().join("elsewhere");
        make_root(&default);
        make_root(&nested);
        make_root(&elsewhere);

        assert!(!forgettable(&default, &dir));
        assert!(forgettable(&elsewhere, &dir));
        assert!(forgettable(&nested, &dir));
        assert!(forgettable(&dir.join("Deleted"), &dir));
    }

    /// Before libraries each had a folder, the Datalib folder was the
    /// library. Its subfolders are its own then, not libraries to list,
    /// and nothing new goes inside it until it is moved.
    #[test]
    fn a_library_at_the_datalib_folder_is_listed_alone_and_blocks_new_ones_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("Documents"));
        make_root(&dir);
        make_root(&dir.join("looks_like_a_library"));

        assert_eq!(legacy_root(&dir), Some(dir.clone()));
        assert_eq!(paths(&libraries(&f, &dir)), vec![dir.clone()]);
        assert!(!forgettable(&dir, &dir));
        assert_eq!(suggested_name(&dir), "");
        assert_eq!(classify(&dir.join("Work"), &dir), Target::InsideLegacy);
        assert_eq!(classify(&tmp.path().join("elsewhere"), &dir), Target::New);
    }

    #[test]
    fn moving_the_legacy_library_puts_everything_in_default() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("Documents"));
        make_root(&dir);
        std::fs::create_dir_all(dir.join("system")).unwrap();
        std::fs::write(dir.join("system/api-token"), "t").unwrap();
        record_recent(&f, &dir).unwrap();

        let moved = move_legacy(&f, &dir).unwrap();
        let default = dir.join(DEFAULT_NAME);
        assert_eq!(moved, default);
        assert!(default.join("config.toml").is_file());
        assert!(default.join("system/api-token").is_file());
        assert!(!dir.join("config.toml").exists());
        assert_eq!(legacy_root(&dir), None);
        assert_eq!(paths(&libraries(&f, &dir)), vec![default]);
    }

    /// A config that spelled out its own folder as `data_root` would
    /// keep pointing at the old place after the move; that line goes.
    #[test]
    fn moving_drops_a_data_root_naming_the_old_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("Documents"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "data_root = \"{}\"\n\n[[groups]]\nid = \"x\"\ndata_root = \"{}\"\n",
                dir.display(),
                dir.display()
            ),
        )
        .unwrap();
        let moved = move_legacy(&f, &dir).unwrap();
        let text = std::fs::read_to_string(moved.join("config.toml")).unwrap();
        assert_eq!(
            text,
            format!(
                "\n[[groups]]\nid = \"x\"\ndata_root = \"{}\"\n",
                dir.display()
            ),
            "only the top-level line naming the old folder goes"
        );
    }

    /// A `Default` already there (made by hand, say) is never merged
    /// into; the move refuses and touches nothing.
    #[test]
    fn moving_refuses_when_default_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("Documents"));
        make_root(&dir);
        std::fs::create_dir_all(dir.join(DEFAULT_NAME)).unwrap();
        assert!(move_legacy(&f, &dir).is_err());
        assert!(dir.join("config.toml").is_file());
    }

    #[test]
    fn an_empty_folder_is_not_a_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_data_root(tmp.path()));
        assert!(!is_data_root(&tmp.path().join("missing")));

        make_root(&tmp.path().join("with_toml"));
        assert!(is_data_root(&tmp.path().join("with_toml")));
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
    fn the_first_library_is_suggested_as_default_and_the_next_has_no_suggestion() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = libraries_dir(&tmp.path().join("Documents"));
        assert_eq!(suggested_name(&dir), DEFAULT_NAME);
        make_root(&dir.join(DEFAULT_NAME));
        assert_eq!(suggested_name(&dir), "");
    }

    #[test]
    fn a_name_goes_in_the_datalib_folder_and_a_path_is_taken_as_it_is() {
        let home = Path::new("/Users/x");
        let dir = Path::new("/Users/x/Documents/Datalib");
        assert_eq!(resolve_new("  ", home, dir), None);
        assert_eq!(resolve_new("Work", home, dir), Some(dir.join("Work")));
        assert_eq!(
            resolve_new(" Photo archive ", home, dir),
            Some(dir.join("Photo archive"))
        );
        assert_eq!(
            resolve_new("~/Work/datalib", home, dir),
            Some(home.join("Work/datalib"))
        );
        assert_eq!(resolve_new("~", home, dir), Some(home.to_path_buf()));
        assert_eq!(
            resolve_new("/Volumes/Archive/lib", home, dir),
            Some(PathBuf::from("/Volumes/Archive/lib"))
        );
    }

    /// A folder holding somebody's files is never adopted and written
    /// into; an empty one, or one not there yet, is where a library goes.
    #[test]
    fn classify_tells_a_new_folder_from_a_library_and_from_someone_elses() {
        let tmp = tempfile::tempdir().unwrap();
        let none = tmp.path().join("none");
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("todo.txt"), "mine").unwrap();
        let lib = tmp.path().join("lib");
        make_root(&lib);
        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();

        assert_eq!(classify(&tmp.path().join("missing"), &none), Target::New);
        assert_eq!(classify(&empty, &none), Target::New);
        assert_eq!(classify(&lib, &none), Target::Library);
        assert_eq!(classify(&notes, &none), Target::Occupied);
        assert_eq!(classify(&file, &none), Target::NotAFolder);
    }

    #[test]
    fn paths_under_home_are_shown_from_the_tilde() {
        let home = Path::new("/Users/x");
        assert_eq!(
            tilde(Path::new("/Users/x/Documents/Datalib/Default"), home),
            "~/Documents/Datalib/Default"
        );
        assert_eq!(tilde(home, home), "~");
        assert_eq!(tilde(Path::new("/Volumes/A"), home), "/Volumes/A");
    }

    #[test]
    fn a_root_is_listed_under_its_folder_name() {
        assert_eq!(display_name(Path::new("/a/b/Work Library")), "Work Library");
        assert_eq!(display_name(Path::new("/")), "/");
    }

    #[test]
    fn the_summary_is_read_from_the_server_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(summary(tmp.path()), None);
        std::fs::create_dir_all(tmp.path().join("system")).unwrap();
        std::fs::write(
            tmp.path().join("system/library-summary.json"),
            r#"{"sources": 4, "bytes": 1024, "last_synced_at": null}"#,
        )
        .unwrap();
        assert_eq!(summary(tmp.path()).unwrap()["sources"], 4);
    }
}
