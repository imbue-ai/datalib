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

/// Where a library given by name lives: `Datalib` in the home
/// directory. Not in Documents, Desktop or Downloads: macOS asks the
/// person for permission before an app reads those.
pub fn libraries_dir(home: &Path) -> PathBuf {
    home.join("Datalib")
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

/// Whether the screen offers to forget `root`. A library in the libraries
/// folder is listed because it is there, not because it was opened, so
/// it stays while it is there; one elsewhere is listed only from the
/// recent list, and so is one whose folder is gone.
pub fn forgettable(root: &Path, libraries_dir: &Path) -> bool {
    root.parent() != Some(libraries_dir) || !is_data_root(root)
}

// TODO(after 2026-12-09): remove the move of libraries out of the
// Documents folder (`libraries_in_documents`, `move_from_documents`,
// `drop_own_data_root`, `replace_recent`, the launcher's
// `launcher_move_from_documents` and the screen's `#moved` line). By
// then the libraries made while they went in Documents will have been
// moved.

/// The folder libraries went in before [`libraries_dir`]: `Datalib` in
/// the platform's Documents directory, which the caller resolves.
pub fn documents_libraries_dir(documents: &Path) -> PathBuf {
    documents.join("Datalib")
}

/// The recent libraries that are in `old_dir`, or are `old_dir` itself
/// (the one library made before libraries each got a folder). Only the
/// recent list is consulted, never a listing of `old_dir`: macOS asks
/// the person for permission when an app reads Documents, and the
/// recent ones are read anyway to list them.
pub fn libraries_in_documents(recents_file: &Path, old_dir: &Path) -> Vec<PathBuf> {
    let recents = parse_recents(&std::fs::read_to_string(recents_file).unwrap_or_default());
    let mut found: Vec<PathBuf> = recents
        .into_iter()
        .filter(|p| p == old_dir || p.parent() == Some(old_dir))
        .filter(|p| is_data_root(p))
        .collect();
    found.sort();
    found
}

/// Move every library [`libraries_in_documents`] names into
/// `libraries_dir` under its own name, and point the recent list at
/// the new places. A library that is `old_dir` itself goes to
/// `Default`. One whose name is taken stays where it is. Returns the
/// new folders. The caller makes sure no library is open.
pub fn move_from_documents(
    recents_file: &Path,
    old_dir: &Path,
    libraries_dir: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    let mut moved = Vec::new();
    for from in libraries_in_documents(recents_file, old_dir) {
        let to = if from == old_dir {
            libraries_dir.join(DEFAULT_NAME)
        } else {
            libraries_dir.join(from.file_name().expect("a library's folder has a name"))
        };
        // A library at `old_dir` takes the folders inside it along.
        if to.exists() || !from.exists() {
            continue;
        }
        std::fs::create_dir_all(libraries_dir)?;
        std::fs::rename(&from, &to)?;
        drop_own_data_root(&to.join("config.toml"), &from)?;
        replace_recent(recents_file, &from, &to)?;
        moved.push(to);
    }
    Ok(moved)
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

fn replace_recent(file: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let roots: Vec<PathBuf> = parse_recents(&existing)
        .into_iter()
        .map(|p| if p == from { to.to_path_buf() } else { p })
        .collect();
    write_recents(file, &roots)
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
/// still the person's.
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
    if libraries_dir.join(DEFAULT_NAME).exists() {
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
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::New => "new",
            Target::Library => "library",
            Target::Occupied => "occupied",
            Target::NotAFolder => "not_a_folder",
        }
    }
}

pub fn classify(dir: &Path) -> Target {
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

    /// A library made in the libraries folder and never opened again is
    /// listed after the recent ones; one that is both is listed once.
    #[test]
    fn libraries_in_the_libraries_folder_are_listed_after_the_recent_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("home"));
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

    /// The libraries folder's libraries are always listed, so forgetting
    /// one would not stick; one whose folder is gone can be, or it would
    /// sit on the list for good.
    #[test]
    fn only_a_library_outside_the_libraries_folder_or_gone_can_be_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = libraries_dir(&tmp.path().join("home"));
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

    /// Only a library on the recent list moves. Each keeps its name, and
    /// the recent list keeps its order and names the new places.
    #[test]
    fn moving_from_documents_brings_each_library_and_repoints_the_recent_list() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let old = documents_libraries_dir(&tmp.path().join("Documents"));
        let dir = libraries_dir(&tmp.path().join("home"));
        let elsewhere = tmp.path().join("elsewhere");
        make_root(&old.join("Work"));
        make_root(&old.join(DEFAULT_NAME));
        make_root(&old.join("Never opened"));
        make_root(&old.join("Gone"));
        make_root(&elsewhere);
        for root in ["Work", "Gone", DEFAULT_NAME] {
            record_recent(&f, &old.join(root)).unwrap();
        }
        record_recent(&f, &elsewhere).unwrap();
        std::fs::remove_dir_all(old.join("Gone")).unwrap();
        assert_eq!(
            libraries_in_documents(&f, &old),
            vec![old.join(DEFAULT_NAME), old.join("Work")]
        );

        let moved = move_from_documents(&f, &old, &dir).unwrap();
        assert_eq!(moved, vec![dir.join(DEFAULT_NAME), dir.join("Work")]);
        assert!(dir.join("Work/config.toml").is_file());
        assert!(!old.join("Work").exists());
        assert!(old.join("Never opened").is_dir());
        assert_eq!(
            paths(&libraries(&f, &dir)),
            vec![
                elsewhere,
                dir.join(DEFAULT_NAME),
                old.join("Gone"),
                dir.join("Work")
            ]
        );
        assert!(libraries_in_documents(&f, &old).is_empty());
    }

    /// Before libraries each had a folder, the Datalib folder was the
    /// library; it becomes `Default`.
    #[test]
    fn a_library_at_the_old_folder_itself_moves_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let old = documents_libraries_dir(&tmp.path().join("Documents"));
        let dir = libraries_dir(&tmp.path().join("home"));
        make_root(&old);
        std::fs::create_dir_all(old.join("system")).unwrap();
        std::fs::write(old.join("system/api-token"), "t").unwrap();
        record_recent(&f, &old).unwrap();

        let moved = move_from_documents(&f, &old, &dir).unwrap();
        assert_eq!(moved, vec![dir.join(DEFAULT_NAME)]);
        assert!(dir.join(DEFAULT_NAME).join("system/api-token").is_file());
        assert!(!old.exists());
    }

    /// A config that spelled out its own folder as `data_root` would
    /// keep pointing at the old place after the move; that line goes.
    #[test]
    fn moving_drops_a_data_root_naming_the_old_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let old = documents_libraries_dir(&tmp.path().join("Documents"));
        let dir = libraries_dir(&tmp.path().join("home"));
        let work = old.join("Work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("config.toml"),
            format!(
                "data_root = \"{}\"\n\n[[groups]]\nid = \"x\"\ndata_root = \"{}\"\n",
                work.display(),
                work.display()
            ),
        )
        .unwrap();
        record_recent(&f, &work).unwrap();
        move_from_documents(&f, &old, &dir).unwrap();
        let text = std::fs::read_to_string(dir.join("Work/config.toml")).unwrap();
        assert_eq!(
            text,
            format!(
                "\n[[groups]]\nid = \"x\"\ndata_root = \"{}\"\n",
                work.display()
            ),
            "only the top-level line naming the old folder goes"
        );
    }

    /// A name already taken in the libraries folder is never merged
    /// into; that library stays in Documents.
    #[test]
    fn moving_leaves_a_library_whose_name_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let old = documents_libraries_dir(&tmp.path().join("Documents"));
        let dir = libraries_dir(&tmp.path().join("home"));
        make_root(&old.join("Work"));
        record_recent(&f, &old.join("Work")).unwrap();
        std::fs::create_dir_all(dir.join("Work")).unwrap();
        assert!(move_from_documents(&f, &old, &dir).unwrap().is_empty());
        assert!(old.join("Work/config.toml").is_file());
    }

    #[test]
    fn no_recent_library_in_documents_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("recent-roots.json");
        let dir = libraries_dir(&tmp.path().join("home"));
        let moved = move_from_documents(&f, &tmp.path().join("Documents/Datalib"), &dir);
        assert!(moved.unwrap().is_empty());
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
        let dir = libraries_dir(&tmp.path().join("home"));
        assert_eq!(suggested_name(&dir), DEFAULT_NAME);
        make_root(&dir.join(DEFAULT_NAME));
        assert_eq!(suggested_name(&dir), "");
    }

    #[test]
    fn a_name_goes_in_the_libraries_folder_and_a_path_is_taken_as_it_is() {
        let home = Path::new("/Users/x");
        let dir = Path::new("/Users/x/Datalib");
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
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("todo.txt"), "mine").unwrap();
        let lib = tmp.path().join("lib");
        make_root(&lib);
        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();

        assert_eq!(classify(&tmp.path().join("missing")), Target::New);
        assert_eq!(classify(&empty), Target::New);
        assert_eq!(classify(&lib), Target::Library);
        assert_eq!(classify(&notes), Target::Occupied);
        assert_eq!(classify(&file), Target::NotAFolder);
    }

    #[test]
    fn paths_under_home_are_shown_from_the_tilde() {
        let home = Path::new("/Users/x");
        assert_eq!(tilde(Path::new("/Users/x/Work/lib"), home), "~/Work/lib");
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
