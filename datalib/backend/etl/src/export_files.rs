//! For a provider that reads a person's unpacked export tree (Facebook,
//! LinkedIn): finding its files, and naming the raw table each lands in.

use std::path::{Path, PathBuf};

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

/// Every file under `root` whose extension is `ext` in any case, sorted.
/// A directory that cannot be read is stepped over.
pub fn files_with_extension(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
