//! Whether this app may read a path a person just chose. On macOS a
//! private folder (Desktop, Documents, cloud storage, a removable volume)
//! lets an app in only once it was chosen in the app's picker, and a grant
//! can lapse; a sync that finds out days later fails where nobody is
//! looking. `docs/dev/wizard_file_pickers.md` §"What the picker buys us in
//! macOS permissions".

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How deep under a chosen folder to look for a file to read. A listing
/// can be allowed while reading a file in it is not, and a source's files
/// usually sit one folder down (`WhatsApp/Databases/`).
const DEPTH: usize = 2;

/// `Ok` when `path` can be listed (a folder) and its first file found
/// within [`DEPTH`] levels read; otherwise what refused, in words.
pub fn check(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| refused(path, &e))?;
    if meta.is_file() {
        return read_a_byte(path);
    }
    match first_file(path, DEPTH)? {
        Some(file) => read_a_byte(&file),
        None => Ok(()),
    }
}

fn first_file(dir: &Path, depth: usize) -> Result<Option<PathBuf>, String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| refused(dir, &e))?
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let (files, dirs): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|e| e.file_type().is_ok_and(|t| t.is_file()));
    if let Some(f) = files.first() {
        return Ok(Some(f.path()));
    }
    if depth == 0 {
        return Ok(None);
    }
    for d in dirs
        .iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
    {
        if let Some(f) = first_file(&d.path(), depth - 1)? {
            return Ok(Some(f));
        }
    }
    Ok(None)
}

fn read_a_byte(file: &Path) -> Result<(), String> {
    let mut byte = [0u8; 1];
    fs::File::open(file)
        .and_then(|mut f| f.read(&mut byte))
        .map(|_| ())
        .map_err(|e| refused(file, &e))
}

fn refused(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readable_folder_passes_through_its_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("Databases")).unwrap();
        fs::write(dir.path().join("Databases/msgstore.db.crypt15"), b"x").unwrap();
        assert_eq!(check(dir.path()), Ok(()));
        assert_eq!(
            check(&dir.path().join("Databases/msgstore.db.crypt15")),
            Ok(())
        );
    }

    #[test]
    fn a_missing_path_says_which() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let err = check(&gone).unwrap_err();
        assert!(err.starts_with(&gone.display().to_string()), "{err}");
    }

    /// A file the process may not open fails the folder that holds it,
    /// which a bare listing would have passed.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_fails_its_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Databases/msgstore.db.crypt15");
        fs::create_dir(dir.path().join("Databases")).unwrap();
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&file).is_ok() {
            return; // root reads anything
        }
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("msgstore.db.crypt15"), "{err}");
    }
}
