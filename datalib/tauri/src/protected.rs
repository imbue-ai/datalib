//! The protected folder a library can live in on macOS: Datalib's app
//! group container, which another app (Terminal, an AI agent's shell,
//! Finder) can open only after the person says yes. What it is and what
//! is still unknown about it: README.md, "A protected library".
//!
//! Nothing here may reference `tauri`: `//datalib/tauri:protected_test`
//! compiles this file on its own.

use std::path::{Component, Path, PathBuf};

/// The app group `Datalib.entitlements` names. macOS lets an app into a
/// group container without asking only when the group's name starts
/// with the Team ID that signed the app: Imbue's.
pub const GROUP_ID: &str = "LDDYAR29MP.com.imbue.datalib";

/// Where macOS keeps the group container for an app that is not
/// sandboxed. Only the app's own call to macOS creates it
/// (`NSFileManager::containerURLForSecurityApplicationGroupIdentifier`).
pub fn container(home: &Path) -> PathBuf {
    home.join("Library").join("Group Containers").join(GROUP_ID)
}

pub fn libraries_dir(container: &Path) -> PathBuf {
    container.join("Libraries")
}

/// latchkey's store for every protected library, in place of
/// `~/.latchkey`: the server is started with `LATCHKEY_DIRECTORY` naming
/// it, and every latchkey the library runs inherits that.
pub fn latchkey_dir(container: &Path) -> PathBuf {
    container.join("latchkey")
}

/// `root` is inside the container, judged on the path's text: a `..`
/// anywhere in it counts as outside, since it could lead anywhere.
pub fn is_protected(root: &Path, container: &Path) -> bool {
    root.starts_with(container) && !root.components().any(|c| c == Component::ParentDir)
}

/// The folder a protected library named `input` goes in. Only a plain
/// name: a protected library cannot go in a folder the person picks.
/// `None` for an empty field or anything else.
pub fn resolve_name(input: &str, libraries_dir: &Path) -> Option<PathBuf> {
    let name = input.trim();
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('~')
        && !name.contains('/');
    plain.then(|| libraries_dir.join(name))
}

/// Give the protected libraries a latchkey store the first time one
/// opens: a copy of `from` (`~/.latchkey`), so their sources keep the
/// sign-ins they had. The originals stay where they were; signing out of
/// them is the person's call. Built beside `to` and renamed into place,
/// so a copy cut short is never taken for a store. A store already at
/// `to` is left alone. Returns whether anything was copied.
pub fn seed_latchkey(from: &Path, to: &Path) -> std::io::Result<bool> {
    if to.exists() {
        return Ok(false);
    }
    let staging = to.with_extension("partial");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    create_owner_only_dir(&staging)?;
    let copied = from.is_dir();
    if copied {
        copy_tree(from, &staging)?;
    }
    std::fs::rename(&staging, to)?;
    Ok(copied)
}

/// Folders and regular files; a symlink is skipped rather than followed
/// out of the store. `fs::copy` keeps each file's mode, which latchkey
/// sets owner-only.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            create_owner_only_dir(&dest)?;
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn create_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_is_named_for_the_team_prefixed_group() {
        assert_eq!(
            container(Path::new("/Users/x")),
            PathBuf::from("/Users/x/Library/Group Containers/LDDYAR29MP.com.imbue.datalib")
        );
    }

    /// The latchkey store goes in the container for a library in it,
    /// and only for one: `..` must not walk a library elsewhere in.
    #[test]
    fn only_a_root_inside_the_container_is_protected() {
        let c = container(Path::new("/Users/x"));
        assert!(is_protected(&libraries_dir(&c).join("Mail"), &c));
        assert!(!is_protected(Path::new("/Users/x/Datalib/Mail"), &c));
        assert!(!is_protected(
            &c.join("Libraries/../../../../Datalib/Mail"),
            &c
        ));
        assert!(!is_protected(
            Path::new("/Users/x/Library/Group Containers/LDDYAR29MP.com.imbue.datalib-other/Mail"),
            &c
        ));
    }

    #[test]
    fn a_protected_library_takes_a_plain_name_only() {
        let dir = Path::new("/c/Libraries");
        assert_eq!(resolve_name(" Mail ", dir), Some(dir.join("Mail")));
        assert_eq!(
            resolve_name("Photo archive", dir),
            Some(dir.join("Photo archive"))
        );
        for not_a_name in ["", "  ", ".", "..", "~", "~/Work", "/Volumes/A", "a/b"] {
            assert_eq!(resolve_name(not_a_name, dir), None, "{not_a_name:?}");
        }
    }

    /// The first protected library starts with the sign-ins latchkey
    /// already holds, keeps their owner-only modes, and leaves the
    /// originals in place; a symlink in the store is not followed.
    #[cfg(unix)]
    #[test]
    fn seeding_copies_the_store_once_and_leaves_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join(".latchkey");
        std::fs::create_dir_all(from.join("plugins/garmin")).unwrap();
        std::fs::write(from.join("credentials.json.enc"), "sealed").unwrap();
        std::fs::set_permissions(
            from.join("credentials.json.enc"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(from.join("plugins/garmin/package.json"), "{}").unwrap();
        std::os::unix::fs::symlink("/etc", from.join("elsewhere")).unwrap();
        let to = latchkey_dir(&tmp.path().join("container"));
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();

        assert!(seed_latchkey(&from, &to).unwrap());
        assert_eq!(
            std::fs::read_to_string(to.join("credentials.json.enc")).unwrap(),
            "sealed"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&to.join("credentials.json.enc")), 0o600);
        assert_eq!(mode(&to), 0o700);
        assert!(to.join("plugins/garmin/package.json").is_file());
        assert!(!to.join("elsewhere").exists());
        assert!(
            from.join("credentials.json.enc").is_file(),
            "the original stays"
        );
        assert!(!to.with_extension("partial").exists());

        std::fs::write(to.join("credentials.json.enc"), "signed in since").unwrap();
        assert!(
            !seed_latchkey(&from, &to).unwrap(),
            "a store already there is kept"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("credentials.json.enc")).unwrap(),
            "signed in since"
        );
    }

    /// Someone who never used latchkey still gets a store, empty, so the
    /// protected libraries never fall back to `~/.latchkey`.
    #[test]
    fn seeding_with_no_store_to_copy_makes_an_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        let to = latchkey_dir(tmp.path());
        assert!(!seed_latchkey(&tmp.path().join("missing"), &to).unwrap());
        assert!(to.is_dir());
        assert_eq!(std::fs::read_dir(&to).unwrap().count(), 0);
    }

    /// A copy cut short leaves a `.partial` folder; the next try starts
    /// it again rather than taking it for a store.
    #[test]
    fn a_copy_cut_short_is_started_again() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join(".latchkey");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("config.json"), "{}").unwrap();
        let to = latchkey_dir(&tmp.path().join("c"));
        std::fs::create_dir_all(to.with_extension("partial")).unwrap();
        std::fs::write(to.with_extension("partial").join("half"), "x").unwrap();
        assert!(seed_latchkey(&from, &to).unwrap());
        assert!(to.join("config.json").is_file());
        assert!(!to.join("half").exists());
    }
}
