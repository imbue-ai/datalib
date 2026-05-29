//! Shared utilities for binary attachments ("blobs") across providers.
//!
//! Blob bytes live in each provider's doltlite raw db. At translate
//! time they're materialized to disk next to the rendered markdown
//! (`rendered_md/<provider>/.../<entity>/blobs/<file_name>`); the
//! markdown links them with a sibling-relative `blobs/<file_name>` so
//! the entity directory is self-contained.
//!
//! This module just holds the shared filename sanitizer used by every
//! provider's renderer so links match the files on disk.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_basic() {
        assert_eq!(safe_filename(Some("Foo Bar.png"), "fb"), "Foo Bar.png");
        assert_eq!(safe_filename(Some("hi/there?"), "fb"), "hi_there_");
        assert_eq!(safe_filename(None, "fb"), "fb");
        assert_eq!(safe_filename(Some(""), "fb"), "fb");
        assert_eq!(safe_filename(Some("   "), "fb"), "fb");
    }
}
