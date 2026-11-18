//! A secret one of datalib's processes hands another. It goes through a
//! private file, never an environment variable or an argument: `ps eww`
//! shows both to any other process of the same user. Keep the file in the
//! data root, beside what the secret opens: anywhere else it is a way around
//! a protected library's folder. `docs/dev/security_posture.md` §"Another
//! program running as you" says why that is enough.

use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Result, Write};
use std::path::Path;

/// Replaces whatever is at `path`. Owner-only from the first byte, and
/// never through a link someone left there.
pub fn write(path: &Path, secret: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(secret.as_bytes())
}

/// Without the newline an editor or `echo` adds, for a file made by hand.
pub fn read(path: &Path) -> Result<String> {
    let secret = std::fs::read_to_string(path)?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "the file is empty"));
    }
    Ok(secret.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system/secret");
        write(&path, "s3cret").unwrap();
        assert_eq!(read(&path).unwrap(), "s3cret");
        write(&path, "another").unwrap();
        assert_eq!(read(&path).unwrap(), "another");
    }

    /// A file made loose by someone else must not keep its mode when the
    /// secret goes in.
    #[cfg(unix)]
    #[test]
    fn the_file_is_private_even_over_a_loose_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write(&path, "s3cret").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A link left at the path must not carry the secret somewhere else.
    #[cfg(unix)]
    #[test]
    fn a_link_at_the_path_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let path = dir.path().join("secret");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

        write(&path, "s3cret").unwrap();

        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "untouched");
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_file_made_by_hand_reads_without_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "s3cret\n").unwrap();
        assert_eq!(read(&path).unwrap(), "s3cret");
    }

    #[test]
    fn an_empty_or_missing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        assert_eq!(read(&path).unwrap_err().kind(), ErrorKind::NotFound);
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), ErrorKind::InvalidData);
    }
}
