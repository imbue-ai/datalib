//! Shared utilities for binary attachments ("media") across providers.
//!
//! Conventions:
//!
//! - Every downloader writes its attachments under
//!   `<data_root>/raw/<source_name>/media/<file_id>/<file_name>`.
//!   `file_id` is whatever the upstream API calls the immutable handle
//!   for the file (Slack's `F0...`, ChatGPT's `file_...`, Anthropic's
//!   per-file UUID). `file_name` is the original filename, sanitized.
//! - Every renderer points markdown at the local copy with a forward-
//!   slash relative path computed by [`relative_link`], so the rendered
//!   `.md` stays clickable when previewed against the same `data_root`.

use std::path::{Component, Path, PathBuf};

/// `<data_root>/raw/<source_name>/media`. All per-source media lives
/// under here.
pub fn raw_media_dir(data_root: &Path, source_name: &str) -> PathBuf {
    data_root.join("raw").join(source_name).join("media")
}

/// `<data_root>/raw/<source_name>/media/<file_id>`. Each file gets its
/// own directory keyed by the upstream id so multiple variants
/// (preview/thumbnail/original) can coexist if a provider exposes them.
pub fn raw_media_file_dir(data_root: &Path, source_name: &str, file_id: &str) -> PathBuf {
    raw_media_dir(data_root, source_name).join(file_id)
}

/// `<data_root>/raw/<source_name>/media/<file_id>/<file_name>`.
pub fn raw_media_file_path(
    data_root: &Path,
    source_name: &str,
    file_id: &str,
    file_name: &str,
) -> PathBuf {
    raw_media_file_dir(data_root, source_name, file_id).join(file_name)
}

/// Relative-to-`data_root` POSIX path for a media file. Useful when you
/// need a path for [`relative_link`] without resolving against the
/// actual `data_root`.
pub fn media_relpath(source_name: &str, file_id: &str, file_name: &str) -> PathBuf {
    PathBuf::from("raw")
        .join(source_name)
        .join("media")
        .join(file_id)
        .join(file_name)
}

/// Markdown filename sanitizer. Replace anything that isn't
/// alphanumeric, `-`, `.`, `_`, or space with `_`, and fall back to
/// `fallback` (typically the file id) when the input is empty.
pub fn safe_filename(name: Option<&str>, fallback: &str) -> String {
    let s = match name {
        Some(n) if !n.is_empty() => n,
        _ => return fallback.to_string(),
    };
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '.' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Relative POSIX path from a markdown file to another file, both
/// expressed as paths *relative to the same root* (typically
/// `data_root`). Returned with `/` separators so the link survives on
/// Windows previews too.
///
/// `from_md` is the path *to* the markdown file (not its parent);
/// directory walking is done internally.
pub fn relative_link(from_md: &Path, to_target: &Path) -> String {
    let from_dir = from_md.parent().unwrap_or(Path::new(""));
    let from_comps: Vec<String> = path_components(from_dir);
    let to_comps: Vec<String> = path_components(to_target);
    let common = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_comps.len() - common;
    let mut out: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
    for c in &to_comps[common..] {
        out.push(c.clone());
    }
    if out.is_empty() {
        ".".to_string()
    } else {
        out.join("/")
    }
}

fn path_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            Component::CurDir => None,
            // Treat absolute/parent components as opaque labels; relative
            // paths under data_root won't hit these in practice, but
            // keeping them addressable avoids silently collapsing
            // unexpected shapes.
            Component::ParentDir => Some("..".to_string()),
            Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => Some(String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_link_sibling() {
        // md at a/b.md → target a/c.png → "c.png"
        let link = relative_link(Path::new("a/b.md"), Path::new("a/c.png"));
        assert_eq!(link, "c.png");
    }

    #[test]
    fn rel_link_up_and_over() {
        // md at rendered_md/slack/T/C/threads/x.md → target raw/src/media/F/y.png
        let md = Path::new("rendered_md/slack/T/C/threads/x.md");
        let tgt = Path::new("raw/src/media/F/y.png");
        let link = relative_link(md, tgt);
        assert_eq!(link, "../../../../../raw/src/media/F/y.png");
    }

    #[test]
    fn rel_link_chatgpt_depth() {
        // md at rendered_md/openai/<acct>/llm_chats/<id>.md → target raw/<src>/media/<id>/<name>
        let md = Path::new("rendered_md/openai/acct/llm_chats/c.md");
        let tgt = Path::new("raw/chatgpt-api/media/file_x/pic.png");
        let link = relative_link(md, tgt);
        assert_eq!(link, "../../../../raw/chatgpt-api/media/file_x/pic.png");
    }

    #[test]
    fn safe_filename_basic() {
        assert_eq!(safe_filename(Some("Foo Bar.png"), "fb"), "Foo Bar.png");
        assert_eq!(safe_filename(Some("hi/there?"), "fb"), "hi_there_");
        assert_eq!(safe_filename(None, "fb"), "fb");
        assert_eq!(safe_filename(Some(""), "fb"), "fb");
        assert_eq!(safe_filename(Some("   "), "fb"), "fb");
    }

    #[test]
    fn media_relpath_shape() {
        let p = media_relpath("tiny-slack", "F_X", "file.png");
        assert_eq!(
            p.to_str().unwrap().replace('\\', "/"),
            "raw/tiny-slack/media/F_X/file.png"
        );
    }
}
