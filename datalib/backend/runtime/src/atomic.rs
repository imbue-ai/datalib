//! Replacing a file in one rename, so a reader sees the old bytes or the
//! new ones and never half of either. Every write-then-rename in the tree
//! goes through here; `scripts/lint_repo.py` check 14 refuses a new
//! hand-rolled one.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// The new file's mode is what `std::fs::write` would give it: the umask
/// decides.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, 0o666, |f| f.write_all(bytes))
}

/// For a file holding credentials: 0600 whatever the umask says.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, 0o600, |f| f.write_all(bytes))
}

/// `fill` writes the content; an error from it leaves `path` as it was
/// and removes the temp file.
pub fn write_with<E: From<io::Error>>(
    path: &Path,
    fill: impl FnOnce(&mut File) -> Result<(), E>,
) -> Result<(), E> {
    replace(path, 0o666, fill)
}

fn replace<E: From<io::Error>>(
    path: &Path,
    mode: u32,
    fill: impl FnOnce(&mut File) -> Result<(), E>,
) -> Result<(), E> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        )
    })?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    // A fresh name per write, so two writers never share a temp file. The
    // `.tmp` suffix is what datalib-http's root watcher ignores.
    let prefix = format!(".{}.", name.to_string_lossy());
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut tmp = builder.tempfile_in(dir)?;
    fill(tmp.as_file_mut())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(entries(dir.path()), ["doc.json"]);
    }

    /// The temp file is 0600 by default; a plain write must not inherit that.
    #[cfg(unix)]
    #[test]
    fn a_plain_write_gets_the_mode_fs_write_would() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let ours = dir.path().join("ours");
        let std_write = dir.path().join("std");
        write(&ours, b"x").unwrap();
        std::fs::write(&std_write, b"x").unwrap();
        assert_eq!(mode(&ours), mode(&std_write));

        let secret = dir.path().join("secret");
        write_owner_only(&secret, b"x").unwrap();
        assert_eq!(mode(&secret), 0o600);
    }

    #[test]
    fn a_failed_fill_keeps_the_old_file_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        write(&path, b"old").unwrap();
        let err = write_with(&path, |f| {
            f.write_all(b"half")?;
            Err(io::Error::other("the download was cut off"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "the download was cut off");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(entries(dir.path()), ["doc.json"]);
    }

    /// Guards the fixed-temp-name copies this replaced: two writers on one
    /// path shared a temp file, so one rename found it gone and a reader
    /// could see one writer's bytes cut into the other's.
    #[test]
    fn concurrent_writers_on_one_path_never_tear_it() {
        const WRITERS: u8 = 8;
        const WRITES: usize = 40;
        const LEN: usize = 256 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("doc.json"));
        write(&path, &vec![b'0'; LEN]).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                while !done.load(Ordering::Relaxed) {
                    let bytes = std::fs::read(&*path).unwrap();
                    assert_eq!(bytes.len(), LEN, "a reader saw a partial file");
                    assert!(
                        bytes.iter().all(|b| *b == bytes[0]),
                        "a reader saw two writers' bytes in one file"
                    );
                    reads += 1;
                }
                reads
            })
        };
        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let body = vec![b'a' + w; LEN];
                    for _ in 0..WRITES {
                        write(&path, &body).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(entries(dir.path()), ["doc.json"]);
    }
}
