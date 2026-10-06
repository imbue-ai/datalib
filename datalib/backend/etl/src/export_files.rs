//! For a provider that reads a person's unpacked export tree (Facebook,
//! LinkedIn): finding its files, and naming the raw table each lands in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::download_problems::RunProblem;

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
    /// Walk `root`, following links. An error only when `root` itself
    /// cannot be listed: then there is nothing to mirror, and an empty
    /// walk would read as an export that holds nothing.
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
                let p = match entry {
                    Ok(entry) => entry.path(),
                    Err(e) => {
                        errors.push((dir.clone(), e.to_string()));
                        continue;
                    }
                };
                match std::fs::metadata(&p) {
                    Ok(m) if m.is_dir() => stack.push(p),
                    Ok(m) if m.is_file() => files.push(p),
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

    /// The files whose extension is `ext` in any case, sorted.
    pub fn with_extension<'a>(&'a self, ext: &'a str) -> impl Iterator<Item = &'a Path> + 'a {
        self.files
            .iter()
            .filter(move |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)))
            .map(PathBuf::as_path)
    }

    /// The `listing:files` row a walk with errors leaves, for
    /// [`crate::download_problems::report_run`]; empty for a clean walk.
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
}
