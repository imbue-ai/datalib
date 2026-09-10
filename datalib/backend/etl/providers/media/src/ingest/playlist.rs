//! M3U playlists: the order, and what it points at.

use std::path::{Component, Path, PathBuf};

/// What kind of thing an entry names, before any attempt to find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// Relative to the playlist's own directory.
    Relative,
    /// Rooted: a leading `/`, or a Windows drive letter or UNC path.
    Absolute,
    /// Carries a URL scheme (`http:`, `file:`, `smb:`).
    Url,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Relative => "relative",
            TargetKind::Absolute => "absolute",
            TargetKind::Url => "url",
        }
    }

    fn of(target: &str) -> Self {
        if has_url_scheme(target) {
            return TargetKind::Url;
        }
        let b = target.as_bytes();
        let windows_drive = b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/');
        if target.starts_with('/') || target.starts_with("\\\\") || windows_drive {
            return TargetKind::Absolute;
        }
        TargetKind::Relative
    }
}

/// `scheme:` where scheme is a letter followed by letters, digits,
/// `+`, `-` or `.`. Written out rather than reached for with a URL
/// crate because `C:\Music` must NOT parse as the scheme `c`.
fn has_url_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    if colon < 2 {
        // A one-letter "scheme" is a Windows drive.
        return false;
    }
    let scheme = &s[..colon];
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// One line of a playlist that names something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Zero-based, in file order. The whole point of the format.
    pub position: i64,
    /// Exactly the bytes on the line, trimmed of surrounding
    /// whitespace and nothing else.
    pub target_raw: String,
    pub target_kind: TargetKind,
    /// The title from the preceding `#EXTINF`, if there was one.
    pub ext_title: Option<String>,
    /// The duration from the preceding `#EXTINF`. `-1` in the file
    /// means "unknown" and is stored as `None`.
    pub ext_duration_s: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Playlist {
    /// From a `#PLAYLIST:` directive.
    pub title: Option<String>,
    /// True when the file is an HLS manifest. Its entries are still
    /// parsed, but the caller records the playlist and skips it.
    pub is_hls: bool,
    pub entries: Vec<Entry>,
}

/// Decode UTF-8, falling back to Latin-1 rather than failing.
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

pub fn parse(bytes: &[u8]) -> Playlist {
    let text = decode(bytes);
    let mut pl = Playlist::default();
    let mut pending_title: Option<String> = None;
    let mut pending_duration: Option<i64> = None;

    for raw_line in text.lines() {
        // Strip a UTF-8 BOM and CR left by a Windows writer.
        let line = raw_line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(directive) = line.strip_prefix('#') {
            if directive.starts_with("EXT-X-") {
                pl.is_hls = true;
            } else if let Some(rest) = directive.strip_prefix("EXTINF:") {
                // `#EXTINF:<seconds>,<title>`; the title may itself
                // contain commas, so only the first one separates.
                let (secs, title) = match rest.split_once(',') {
                    Some((s, t)) => (s, Some(t)),
                    None => (rest, None),
                };
                pending_duration = secs
                    // Some writers put attributes after the duration.
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
                    .filter(|d| *d >= 0);
                pending_title = title
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
            } else if let Some(name) = directive.strip_prefix("PLAYLIST:") {
                let name = name.trim();
                if !name.is_empty() {
                    pl.title = Some(name.to_string());
                }
            }
            continue;
        }
        pl.entries.push(Entry {
            position: pl.entries.len() as i64,
            target_kind: TargetKind::of(line),
            target_raw: line.to_string(),
            ext_title: pending_title.take(),
            ext_duration_s: pending_duration.take(),
        });
    }
    pl
}

pub fn resolve(target: &str, kind: TargetKind, playlist_rel: &str) -> Option<String> {
    if kind != TargetKind::Relative {
        return None;
    }
    // Windows separators in a relative target are common in playlists
    // written by desktop players, and mean the same thing.
    let target = target.replace('\\', "/");

    let base = Path::new(playlist_rel).parent().unwrap_or(Path::new(""));
    let joined = base.join(&target);

    // Normalize `.` and `..` textually. `fs::canonicalize` would be
    // wrong twice over: it touches the disk (so a missing target — the
    // interesting case — would resolve to nothing) and it follows
    // symlinks out of the tree.
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` that would climb above the root escapes the
                // tree; there is nothing inside it to point at.
                parts.pop()?;
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
            // A rooted or prefixed component means this was not really
            // a relative path.
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

pub fn format_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("m3u8") => "m3u8",
        _ => "m3u",
    }
}

pub fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel: PathBuf = path.strip_prefix(root).ok()?.to_path_buf();
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"#EXTM3U\n\
#PLAYLIST:Bridge Ambience\n\
#EXTINF:227,Jean-Luc Picard - Ode to Spot\n\
tracks/ode_to_spot.mp3\r\n\
\n\
# a plain comment\n\
#EXTINF:-1,Unknown Length\n\
../shared/warp_core_hum.flac\n\
http://holodeck.local/stream.mp3\n\
/Volumes/Enterprise/tracks/tea_earl_grey.m4a\n\
..\\sibling\\windows_path.mp3\n";

    #[test]
    fn entries_keep_their_order_and_their_raw_text() {
        let pl = parse(SAMPLE);
        assert_eq!(pl.title.as_deref(), Some("Bridge Ambience"));
        assert!(!pl.is_hls);
        let raws: Vec<&str> = pl.entries.iter().map(|e| e.target_raw.as_str()).collect();
        assert_eq!(
            raws,
            vec![
                "tracks/ode_to_spot.mp3",
                "../shared/warp_core_hum.flac",
                "http://holodeck.local/stream.mp3",
                "/Volumes/Enterprise/tracks/tea_earl_grey.m4a",
                "..\\sibling\\windows_path.mp3",
            ]
        );
        assert_eq!(
            pl.entries.iter().map(|e| e.position).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn extinf_duration_and_title_attach_to_the_next_entry_only() {
        let pl = parse(SAMPLE);
        assert_eq!(pl.entries[0].ext_duration_s, Some(227));
        assert_eq!(
            pl.entries[0].ext_title.as_deref(),
            Some("Jean-Luc Picard - Ode to Spot")
        );
        // `-1` means unknown, not a duration of minus one second.
        assert_eq!(pl.entries[1].ext_duration_s, None);
        assert_eq!(pl.entries[1].ext_title.as_deref(), Some("Unknown Length"));
        // The third entry had no #EXTINF of its own.
        assert_eq!(pl.entries[2].ext_title, None);
        assert_eq!(pl.entries[2].ext_duration_s, None);
    }

    #[test]
    fn a_title_containing_a_comma_is_not_split_at_the_second_one() {
        let pl = parse(b"#EXTINF:180,Bach, Johann Sebastian - Air\ntrack.mp3\n");
        assert_eq!(
            pl.entries[0].ext_title.as_deref(),
            Some("Bach, Johann Sebastian - Air")
        );
    }

    #[test]
    fn target_kinds_separate_relative_absolute_and_url() {
        let pl = parse(SAMPLE);
        let kinds: Vec<TargetKind> = pl.entries.iter().map(|e| e.target_kind).collect();
        assert_eq!(
            kinds,
            vec![
                TargetKind::Relative,
                TargetKind::Relative,
                TargetKind::Url,
                TargetKind::Absolute,
                TargetKind::Relative,
            ]
        );
    }

    #[test]
    fn a_windows_drive_letter_is_a_path_not_a_url_scheme() {
        assert_eq!(TargetKind::of("C:\\Music\\song.mp3"), TargetKind::Absolute);
        assert_eq!(TargetKind::of("D:/Music/song.mp3"), TargetKind::Absolute);
        assert_eq!(
            TargetKind::of("\\\\nas\\music\\song.mp3"),
            TargetKind::Absolute
        );
        assert_eq!(TargetKind::of("smb://nas/music/song.mp3"), TargetKind::Url);
        assert_eq!(TargetKind::of("file:///music/song.mp3"), TargetKind::Url);
    }

    #[test]
    fn hls_manifests_are_flagged_by_their_own_tags() {
        let hls = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n\
#EXTINF:9.009,\nsegment0.ts\n";
        assert!(parse(hls).is_hls);
        // A real playlist with the same extension is not.
        assert!(!parse(SAMPLE).is_hls);
    }

    #[test]
    fn resolution_is_relative_to_the_playlists_own_directory() {
        assert_eq!(
            resolve(
                "tracks/ode_to_spot.mp3",
                TargetKind::Relative,
                "playlists/bridge.m3u"
            )
            .as_deref(),
            Some("playlists/tracks/ode_to_spot.mp3")
        );
        assert_eq!(
            resolve(
                "../shared/warp_core_hum.flac",
                TargetKind::Relative,
                "playlists/bridge.m3u"
            )
            .as_deref(),
            Some("shared/warp_core_hum.flac")
        );
        assert_eq!(
            resolve("./a/./b.mp3", TargetKind::Relative, "p/x.m3u").as_deref(),
            Some("p/a/b.mp3")
        );
    }

    #[test]
    fn windows_separators_in_a_relative_target_resolve() {
        assert_eq!(
            resolve("..\\sibling\\song.mp3", TargetKind::Relative, "p/x.m3u").as_deref(),
            Some("sibling/song.mp3")
        );
    }

    #[test]
    fn climbing_out_of_the_root_resolves_to_nothing() {
        // The entry is still recorded; only `resolved_path` is NULL.
        assert_eq!(
            resolve("../../outside.mp3", TargetKind::Relative, "p/x.m3u"),
            None
        );
        assert_eq!(
            resolve("../outside.mp3", TargetKind::Relative, "x.m3u"),
            None
        );
    }

    #[test]
    fn urls_and_absolute_paths_never_resolve_into_the_tree() {
        assert_eq!(resolve("http://x/y.mp3", TargetKind::Url, "p/x.m3u"), None);
        assert_eq!(
            resolve("/etc/passwd", TargetKind::Absolute, "p/x.m3u"),
            None
        );
        // …and a traversal dressed as a relative path cannot sneak in.
        assert_eq!(resolve("/a/b.mp3", TargetKind::Relative, "p/x.m3u"), None);
    }

    #[test]
    fn latin1_bytes_decode_instead_of_failing() {
        // 0xE9 is `é` in Latin-1 and invalid on its own in UTF-8.
        let pl = parse(b"#EXTINF:1,Caf\xe9\ncaf\xe9.mp3\n");
        assert_eq!(pl.entries[0].target_raw, "café.mp3");
        assert_eq!(pl.entries[0].ext_title.as_deref(), Some("Café"));
    }

    #[test]
    fn utf8_is_preferred_when_the_bytes_are_valid() {
        let pl = parse("#EXTINF:1,Café\ncafé.mp3\n".as_bytes());
        assert_eq!(pl.entries[0].target_raw, "café.mp3");
    }

    #[test]
    fn a_bom_does_not_become_part_of_the_first_directive() {
        let pl = parse("\u{feff}#EXTM3U\n#PLAYLIST:Named\ntrack.mp3\n".as_bytes());
        assert_eq!(pl.title.as_deref(), Some("Named"));
        assert_eq!(pl.entries.len(), 1);
    }

    #[test]
    fn a_bare_playlist_with_no_directives_still_parses() {
        let pl = parse(b"one.mp3\ntwo.mp3\nthree.mp3\n");
        assert_eq!(pl.entries.len(), 3);
        assert_eq!(pl.entries[2].position, 2);
        assert!(pl.title.is_none());
    }

    #[test]
    fn duplicate_entries_are_kept_because_the_order_is_the_content() {
        let pl = parse(b"a.mp3\nb.mp3\na.mp3\n");
        assert_eq!(pl.entries.len(), 3);
        assert_eq!(pl.entries[0].target_raw, pl.entries[2].target_raw);
        assert_ne!(pl.entries[0].position, pl.entries[2].position);
    }

    #[test]
    fn format_comes_from_the_extension() {
        assert_eq!(format_of(Path::new("a.m3u8")), "m3u8");
        assert_eq!(format_of(Path::new("a.M3U8")), "m3u8");
        assert_eq!(format_of(Path::new("a.m3u")), "m3u");
    }
}
