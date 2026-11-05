//! The protected folder a library can live in on macOS: Datalib's app
//! group container, which another app (Terminal, an AI agent's shell,
//! Finder) can open only after the person says yes. What it is and what
//! is still unknown about it: README.md, "A protected library".
//!
//! Only this folder guards a library's sign-ins. Any program can get the
//! keychain key by running the app's Node, so a change that lets the store
//! be read from outside gives away the sign-ins.
//!
//! Nothing here may reference `tauri`: `//datalib/tauri:protected_test`
//! compiles this file on its own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// `root` is inside the container, judged by where both paths lead once
/// every `..` and link is followed. A root that cannot be followed counts
/// as inside; nothing is inside a container that is not there.
pub fn is_protected(root: &Path, container: &Path) -> bool {
    let container = match container.canonicalize() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    root.canonicalize()
        .map_or(true, |root| root.starts_with(container))
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

/// The variables a protected library's server starts with, which every
/// latchkey it runs inherits. `root` is the library's real path. latchkey
/// makes the store in the library and its key in the keychain the first
/// time it needs them, so a library starts signed in to nothing and has
/// nothing to do with `~/.latchkey`.
pub fn latchkey_env(root: &Path) -> Vec<(&'static str, OsString)> {
    let mut key_name = OsString::from("Datalib Latchkey - ");
    key_name.push(root.file_name().unwrap_or_default());
    vec![
        (
            "LATCHKEY_DIRECTORY",
            root.join("system").join("latchkey").into_os_string(),
        ),
        ("LATCHKEY_KEYRING_SERVICE_NAME", key_name),
    ]
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
    /// and only for one, judged by where the path really leads: a `..`
    /// or a link can make a path's text say either.
    #[cfg(unix)]
    #[test]
    fn only_a_root_inside_the_container_is_protected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let c = container(home);
        let mail = libraries_dir(&c).join("Mail");
        let outside = home.join("Datalib/Mail");
        std::fs::create_dir_all(&mail).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(
            home.join("Library/Group Containers/LDDYAR29MP.com.imbue.datalib-other/Mail"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&mail, home.join("Datalib/into")).unwrap();
        std::os::unix::fs::symlink(&outside, libraries_dir(&c).join("out")).unwrap();

        assert!(is_protected(&mail, &c));
        assert!(is_protected(&c.join("Libraries/../Libraries/Mail"), &c));
        assert!(
            is_protected(&home.join("Datalib/into"), &c),
            "a link from outside into the container"
        );
        assert!(!is_protected(&outside, &c));
        assert!(!is_protected(
            &c.join("Libraries/../../../../Datalib/Mail"),
            &c
        ));
        assert!(
            !is_protected(&libraries_dir(&c).join("out"), &c),
            "a link in the container to a library outside it"
        );
        assert!(!is_protected(
            &home.join("Library/Group Containers/LDDYAR29MP.com.imbue.datalib-other/Mail"),
            &c
        ));
        assert!(
            is_protected(&libraries_dir(&c).join("Missing"), &c),
            "a path that cannot be followed counts as protected"
        );
        assert!(
            !is_protected(&mail, &container(&home.join("nobody"))),
            "nothing is in a container that is not there"
        );
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

    /// Each library gets its own store and its own keychain item, so
    /// signing in to one reaches no other, and neither names `~/.latchkey`
    /// or latchkey's shared `latchkey` item.
    #[test]
    fn a_protected_library_has_its_own_store_and_keychain_item() {
        let env = latchkey_env(Path::new("/c/Libraries/Photo archive"));
        assert_eq!(
            env,
            [
                (
                    "LATCHKEY_DIRECTORY",
                    OsString::from("/c/Libraries/Photo archive/system/latchkey")
                ),
                (
                    "LATCHKEY_KEYRING_SERVICE_NAME",
                    OsString::from("Datalib Latchkey - Photo archive")
                ),
            ]
        );
        assert_ne!(env, latchkey_env(Path::new("/c/Libraries/Mail")));
    }
}
