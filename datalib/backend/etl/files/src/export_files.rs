//! For a provider that reads a person's unpacked export tree (Facebook,
//! LinkedIn): finding its files, and naming the raw table each lands in.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use datalib_etl::download_problems::RunProblem;

const LINK_REFUSED: &str = "a symbolic link, which an export does not hold; it was not followed";

/// A file itself, not a link to one. For a reader of an export, where a
/// link could point anywhere on the machine.
pub fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// The raw table for an export file, from its path relative to the
/// export root: lowercase, every non-alphanumeric run collapsed to `_`,
/// the extension dropped, and a trailing `_<digits>` dropped too when
/// something stays in front of it. A leading digit gets a `t_` prefix.
pub fn table_name(rel: &str) -> String {
    let stem = Path::new(rel)
        .with_extension("")
        .to_string_lossy()
        .to_lowercase();
    let mut out = String::new();
    let mut prev_us = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    let mut t = out.trim_matches('_').to_string();
    if let Some((head, last)) = t.rsplit_once('_') {
        if !head.is_empty() && !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
            t = head.to_string();
        }
    }
    if t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("t_{t}")
    } else {
        t
    }
}

/// Every file under an export root, and what the walk could not read.
#[derive(Debug)]
pub struct ExportFiles {
    root: PathBuf,
    /// Sorted.
    files: Vec<PathBuf>,
    /// Entries under the root that could not be read, with why. A
    /// directory here hides its files, so while this is non-empty a file
    /// missing from the walk is not evidence that it went.
    pub errors: Vec<(PathBuf, String)>,
}

impl ExportFiles {
    /// Walk `root`. An export can be made by anyone, so a link beneath it
    /// is not followed, to a file or to a folder: it could read anything on
    /// the machine, or loop back on itself. It goes in [`Self::errors`],
    /// because what it stood for is not seen. `root` is what a person
    /// chose, and may itself be a link. An error only when `root` cannot be
    /// listed: then there is nothing to mirror, and an empty walk would
    /// read as an export that holds nothing.
    pub fn walk(root: &Path) -> Result<Self> {
        let mut files = Vec::new();
        let mut errors = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if dir == root => {
                    return Err(e).with_context(|| format!("list {}", root.display()))
                }
                Err(e) => {
                    errors.push((dir, e.to_string()));
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        errors.push((dir.clone(), e.to_string()));
                        continue;
                    }
                };
                let p = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(p),
                    Ok(t) if t.is_file() => files.push(p),
                    Ok(t) if t.is_symlink() => errors.push((p, LINK_REFUSED.to_string())),
                    Ok(_) => {}
                    Err(e) => errors.push((p, e.to_string())),
                }
            }
        }
        files.sort();
        Ok(Self {
            root: root.to_path_buf(),
            files,
            errors,
        })
    }

    /// [`Self::walk`], for a reader that stops at an export it cannot see
    /// whole rather than reading what it can.
    pub fn walk_whole(root: &Path) -> Result<Self> {
        let walk = Self::walk(root)?;
        if let Some((path, error)) = walk.errors.first() {
            bail!("{}: {error}", path.display());
        }
        Ok(walk)
    }

    /// Sorted.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The file a name from inside the export means, if the walk found one
    /// there. A record's text is not to be trusted to stay in the export,
    /// so it is looked up here rather than joined to the root and opened.
    pub fn file(&self, rel: &str) -> Option<&Path> {
        let path = self.root.join(rel);
        let at = self.files.binary_search(&path).ok()?;
        Some(&self.files[at])
    }

    /// The files whose extension is `ext` in any case, sorted.
    pub fn with_extension<'a>(&'a self, ext: &'a str) -> impl Iterator<Item = &'a Path> + 'a {
        self.files
            .iter()
            .filter(move |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)))
            .map(PathBuf::as_path)
    }

    /// The `listing:files` row a walk with errors leaves, for the run's
    /// `RunProblems` (`extend`); empty for a clean walk.
    pub fn walk_problems(&self) -> Vec<RunProblem> {
        let Some((path, error)) = self.errors.first() else {
            return Vec::new();
        };
        vec![RunProblem::listing(
            "files",
            // The path first, and short: the sample is cut at 80 characters.
            format!(
                "{}: {error} ({} unreadable under {}; nothing was deleted this run)",
                path.strip_prefix(&self.root).unwrap_or(path).display(),
                self.errors.len(),
                self.root.display(),
            ),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_files_by_extension_in_any_case_and_depth() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("Top.CSV"), "x").unwrap();
        std::fs::write(d.path().join("a/b/deep.csv"), "x").unwrap();
        std::fs::write(d.path().join("a/other.json"), "x").unwrap();
        let walk = ExportFiles::walk(d.path()).unwrap();
        let csv: Vec<_> = walk.with_extension("csv").collect();
        assert_eq!(
            csv,
            [d.path().join("Top.CSV"), d.path().join("a/b/deep.csv")]
        );
        assert!(walk.errors.is_empty());
        assert!(walk.walk_problems().is_empty());
    }

    /// An export root that is not there is not an empty export.
    #[test]
    fn a_root_that_cannot_be_listed_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(ExportFiles::walk(&d.path().join("not-unpacked-yet")).is_err());
    }

    /// What the walk could not read is said, so a caller can hold back
    /// deleting what it did not see.
    #[cfg(unix)]
    #[test]
    fn an_entry_that_cannot_be_read_is_a_walk_problem() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("kept.json"), "x").unwrap();
        std::os::unix::fs::symlink(d.path().join("nowhere"), d.path().join("album")).unwrap();
        let walk = ExportFiles::walk(d.path()).unwrap();
        assert_eq!(walk.with_extension("json").count(), 1);
        assert_eq!(walk.errors.len(), 1, "{:?}", walk.errors);
        let problems = walk.walk_problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].key(), "listing:files");
    }

    /// A folder in an export that links out of it: what is out there is not
    /// listed, so nothing can read it, and the walk says what it passed.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_folder_outside_the_export_is_not_followed() {
        let top = tempfile::tempdir().unwrap();
        let export = top.path().join("export");
        let outside = top.path().join("outside");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(export.join("kept.json"), "x").unwrap();
        std::fs::write(outside.join("secret.json"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, export.join("album")).unwrap();

        let walk = ExportFiles::walk(&export).unwrap();
        assert_eq!(
            walk.with_extension("json").collect::<Vec<_>>(),
            [export.join("kept.json")]
        );
        assert_eq!(walk.errors.len(), 1, "{:?}", walk.errors);
        assert_eq!(walk.errors[0].0, export.join("album"));
        assert_eq!(walk.walk_problems().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_outside_the_export_is_not_followed() {
        let top = tempfile::tempdir().unwrap();
        let export = top.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(top.path().join("secret.json"), "x").unwrap();
        std::os::unix::fs::symlink(top.path().join("secret.json"), export.join("posts.json"))
            .unwrap();
        let walk = ExportFiles::walk(&export).unwrap();
        assert_eq!(walk.with_extension("json").count(), 0);
        assert_eq!(walk.errors.len(), 1, "{:?}", walk.errors);
    }

    /// A link back to a folder above it would have the walk go round for
    /// as long as the paths stayed short enough to open.
    #[cfg(unix)]
    #[test]
    fn a_link_back_to_an_ancestor_does_not_loop() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/deep.json"), "x").unwrap();
        std::os::unix::fs::symlink(d.path(), d.path().join("a/b/up")).unwrap();
        std::os::unix::fs::symlink(d.path().join("a"), d.path().join("a/b/up2")).unwrap();
        let walk = ExportFiles::walk(d.path()).unwrap();
        assert_eq!(
            walk.with_extension("json").collect::<Vec<_>>(),
            [d.path().join("a/b/deep.json")]
        );
        assert_eq!(walk.errors.len(), 2, "{:?}", walk.errors);
    }

    /// The root is what a person chose: their own link to an export kept on
    /// another disk still reads.
    #[cfg(unix)]
    #[test]
    fn a_root_that_is_itself_a_link_is_walked() {
        let top = tempfile::tempdir().unwrap();
        let real = top.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("posts.json"), "x").unwrap();
        let link = top.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let walk = ExportFiles::walk(&link).unwrap();
        assert_eq!(walk.with_extension("json").count(), 1);
        assert!(walk.errors.is_empty(), "{:?}", walk.errors);
    }

    #[test]
    fn a_name_from_inside_the_export_finds_only_what_the_walk_found() {
        let top = tempfile::tempdir().unwrap();
        let export = top.path().join("export");
        std::fs::create_dir_all(export.join("media")).unwrap();
        std::fs::write(export.join("media/photo.jpg"), "x").unwrap();
        std::fs::write(top.path().join("secret.jpg"), "x").unwrap();
        let walk = ExportFiles::walk(&export).unwrap();
        let found = export.join("media/photo.jpg");
        assert_eq!(walk.file("media/photo.jpg"), Some(found.as_path()));
        let absolute = top.path().join("secret.jpg");
        for name in [
            "../secret.jpg",
            "media/../../secret.jpg",
            absolute.to_str().unwrap(),
            "media",
            "media/gone.jpg",
            "",
        ] {
            assert_eq!(walk.file(name), None, "{name:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_name_cannot_reach_through_a_link() {
        let top = tempfile::tempdir().unwrap();
        let export = top.path().join("export");
        let outside = top.path().join("outside");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.jpg"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, export.join("media")).unwrap();
        let walk = ExportFiles::walk(&export).unwrap();
        assert!(export.join("media/secret.jpg").is_file());
        assert_eq!(walk.file("media/secret.jpg"), None);
    }

    #[cfg(unix)]
    #[test]
    fn walking_whole_refuses_an_export_it_cannot_see_whole() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("kept.json"), "x").unwrap();
        assert_eq!(ExportFiles::walk_whole(d.path()).unwrap().files().len(), 1);
        std::os::unix::fs::symlink(d.path().join("kept.json"), d.path().join("link")).unwrap();
        let e = ExportFiles::walk_whole(d.path()).unwrap_err().to_string();
        assert!(e.contains("symbolic link"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn only_a_file_itself_is_a_regular_file() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(is_regular_file(&file));
        assert!(!is_regular_file(&link));
        assert!(!is_regular_file(d.path()));
        assert!(!is_regular_file(&d.path().join("gone")));
    }
}
