//! Unpacking a fetched `.tar.gz`: the runtime tarball and a package's
//! build share it. Modes and relative symlinks inside the tree are kept,
//! which the pnpm layout and `node/bin/node` both need; a symlink that
//! could point outside the tree is refused.

use std::fs;
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};

/// Unpack a `.tar.gz` into `dest`, which must exist. The tar crate
/// refuses an entry that would land outside `dest` and a hard link to
/// something outside it, but makes a symlink to anywhere; so once it is
/// done every symlink in the tree is checked with [`link_escapes`], and
/// the first that fails is the error. `dest` then holds what was
/// unpacked, for the caller to remove.
pub fn unpack_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut tar = tar::Archive::new(gz);
    tar.set_overwrite(true);
    tar.unpack(dest)
        .with_context(|| format!("unpack {} into {}", archive.display(), dest.display()))?;
    check_links(dest, dest, 0)
        .with_context(|| format!("unpack {} into {}", archive.display(), dest.display()))
}

/// Walk `dir`, `depth` directories below `root`, without following a
/// symlink, and check each one found.
fn check_links(root: &Path, dir: &Path, depth: usize) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            check_links(root, &path, depth + 1)?;
        } else if kind.is_symlink() {
            let target = fs::read_link(&path)?;
            if link_escapes(depth, &target) {
                bail!(
                    "the archive's symlink {} points to {}, which is not a relative path inside it",
                    path.strip_prefix(root).unwrap_or(&path).display(),
                    target.display()
                );
            }
        }
    }
    Ok(())
}

/// Whether a symlink `depth` directories below the tree's root, pointing
/// at `target`, could resolve outside the tree. It stays inside when it
/// is relative, its `..`s all come first and there are no more of them
/// than `depth`: then it climbs to a directory in the tree and descends
/// by names, and a name is either a real entry or another link that
/// passed this check. A `..` after a name is refused because the name
/// may be a link, which `..` would climb out of.
fn link_escapes(depth: usize, target: &Path) -> bool {
    let mut ups = 0;
    let mut descended = false;
    for part in target.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if !descended => ups += 1,
            Component::Normal(_) => descended = true,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    ups > depth
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.tar.gz` of the given `(path, contents)` files and
    /// `(path, target)` symlinks.
    fn archive_of(
        dir: &Path,
        files: &[(&str, &str)],
        links: &[(&str, &str)],
    ) -> std::path::PathBuf {
        let archive = dir.join("a.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        for (path, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, path, body.as_bytes()).unwrap();
        }
        for (path, target) in links {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            tar.append_link(&mut h, path, target).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        archive
    }

    /// Modes and symlinks survive the round trip — a runtime whose
    /// `node` lost its exec bit, or whose pnpm links dereferenced, is
    /// no runtime.
    #[test]
    fn unpack_keeps_modes_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("node/bin")).unwrap();
        fs::write(src.join("node/bin/node"), b"#!/bin/sh\necho node\n").unwrap();
        fs::set_permissions(src.join("node/bin/node"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("node/bin/node", src.join("link")).unwrap();

        let archive = dir.path().join("rt.tar.gz");
        {
            let f = fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            tar.follow_symlinks(false);
            tar.append_dir_all(".", &src).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }

        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        unpack_tar_gz(&archive, &out).unwrap();
        let node = out.join("node/bin/node");
        assert!(fs::metadata(&node).unwrap().permissions().mode() & 0o111 != 0);
        assert!(fs::symlink_metadata(out.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(out.join("link")).unwrap(),
            fs::read(&node).unwrap()
        );
    }

    /// The links a pnpm `node_modules` and Node's `bin/` are made of
    /// unpack: up to the root and no further, then down by name.
    #[test]
    fn links_that_climb_no_higher_than_the_root_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive_of(
            dir.path(),
            &[
                ("lib/node_modules/npm/bin/npm-cli.js", "x"),
                ("node_modules/.pnpm/b@2/node_modules/b/index.js", "x"),
            ],
            &[
                ("bin/npm", "../lib/node_modules/npm/bin/npm-cli.js"),
                (
                    "node_modules/.pnpm/a@1/node_modules/b",
                    "../../b@2/node_modules/b",
                ),
                ("node_modules/b", ".pnpm/b@2/node_modules/b"),
                ("here", "./bin/npm"),
            ],
        );
        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        unpack_tar_gz(&archive, &out).unwrap();
        assert_eq!(fs::read(out.join("bin/npm")).unwrap(), b"x");
        assert_eq!(fs::read(out.join("node_modules/b/index.js")).unwrap(), b"x");
    }

    /// A symlink that is absolute, climbs above the root, or climbs back
    /// up after a name (which may itself be a link) is refused, and the
    /// error names it.
    #[test]
    fn a_link_that_could_point_outside_is_refused() {
        for (at, target) in [
            ("bin/sh", "/bin/sh"),
            ("up", "../outside"),
            ("bin/up", "../../outside"),
            ("bin/through", "../bin/../../outside"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let archive = archive_of(dir.path(), &[("bin/tool", "x")], &[(at, target)]);
            let out = dir.path().join("out");
            fs::create_dir(&out).unwrap();
            let e = format!("{:#}", unpack_tar_gz(&archive, &out).unwrap_err());
            assert!(
                e.contains(at) && e.contains(target),
                "{at} -> {target}: {e}"
            );
        }
    }

    #[test]
    fn link_escapes_counts_leading_ups_against_depth() {
        assert!(!link_escapes(0, Path::new("a/b")));
        assert!(!link_escapes(1, Path::new("../a")));
        assert!(!link_escapes(0, Path::new(".")));
        assert!(link_escapes(0, Path::new("../a")));
        assert!(link_escapes(3, Path::new("/a")));
        assert!(link_escapes(3, Path::new("a/../b")));
    }
}
