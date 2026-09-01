//! What a file *is*: media class, container format, and whether we know
//! how to compute a metadata-excluding payload hash for it.
//!
//! # Two-stage classification, and why
//!
//! The walk decides what to visit from the **extension** alone
//! ([`accept`]), because that decision runs for every entry in the tree
//! and must not cost a `read(2)`. Everything downstream decides from
//! the file's **leading bytes** ([`Container::sniff`]), because
//! extensions lie in ways that matter here:
//!
//! - `.m4a`, `.m4v`, `.mp4`, `.mov` and `.heic` are all ISO base media
//!   files. The extension is a hint about intent; the `ftyp` brand is
//!   the fact. A `.mov` holding an `M4A ` brand is an audio file.
//! - `.jpg` files that are really PNGs are common enough in exported
//!   libraries to be worth not mis-parsing.
//! - `.dng` is a TIFF, and so is `.tif`. One reader serves both.
//!
//! Extension still breaks ties the bytes cannot: a bare `ftyp` brand
//! of `isom` says nothing about whether the file carries video, so the
//! extension picks the class and the track census corrects it later.

use std::path::Path;

/// The three top-level buckets, one per class table. `Audio` rows get a
/// `media_audio` row, `Image` and `Video` share `media_visual` — see
/// `DOWNLOAD.md` §"Two class tables, not three".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaClass {
    Audio,
    Image,
    Video,
}

impl MediaClass {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaClass::Audio => "audio",
            MediaClass::Image => "image",
            MediaClass::Video => "video",
        }
    }
}

/// The container we will actually parse. Distinct from the codec inside
/// it: `Bmff` holds AAC or ALAC or H.264, `Riff` holds PCM or an
/// MJPEG stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp3,
    Flac,
    /// RIFF. `WAVE` and `AVI ` differ only in their form type.
    Wav,
    Avi,
    /// ISO base media file format: MP4, M4A, MOV, HEIC, AVIF.
    Bmff,
    Jpeg,
    Png,
    /// TIFF and its raw descendants, DNG chief among them.
    Tiff,
    Gif,
    /// RIFF again, but a `WEBP` form type.
    Webp,
    Aiff,
    /// Ogg/Opus/Vorbis. Recognized and tagged; no payload hash yet.
    Ogg,
    /// Matroska and WebM. Recognized and tagged; no payload hash yet.
    Matroska,
    /// A file whose extension we index but whose bytes matched nothing
    /// we parse. It still gets a row — knowing the file is there is the
    /// point — with `payload_blake3` NULL.
    Unknown,
}

impl Container {
    pub fn as_str(self) -> &'static str {
        match self {
            Container::Mp3 => "mp3",
            Container::Flac => "flac",
            Container::Wav => "wav",
            Container::Avi => "avi",
            Container::Bmff => "bmff",
            Container::Jpeg => "jpeg",
            Container::Png => "png",
            Container::Tiff => "tiff",
            Container::Gif => "gif",
            Container::Webp => "webp",
            Container::Aiff => "aiff",
            Container::Ogg => "ogg",
            Container::Matroska => "matroska",
            Container::Unknown => "unknown",
        }
    }

    /// Identify from the first bytes of the file. `head` should be at
    /// least [`SNIFF_LEN`] bytes when the file is that long.
    ///
    /// Returns `Unknown` rather than guessing from the extension: a row
    /// that says `unknown` is a true statement about the bytes, where a
    /// row that says `jpeg` because the name ended in `.jpg` is a lie
    /// that every later query inherits.
    pub fn sniff(head: &[u8]) -> Self {
        // ID3v2 tags precede the audio in MP3 and can precede it in
        // several other containers, so this test has to come before the
        // frame-sync one below.
        if head.starts_with(b"ID3") {
            return Container::Mp3;
        }
        if head.starts_with(b"fLaC") {
            return Container::Flac;
        }
        if head.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Container::Png;
        }
        if head.starts_with(&[0xff, 0xd8, 0xff]) {
            return Container::Jpeg;
        }
        if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
            return Container::Gif;
        }
        if head.starts_with(b"II\x2a\x00") || head.starts_with(b"MM\x00\x2a") {
            // Little- and big-endian TIFF. DNG, CR2 and NEF all answer
            // here; DNG is the one whose IFD layout we follow.
            return Container::Tiff;
        }
        if head.starts_with(b"\x1a\x45\xdf\xa3") {
            return Container::Matroska;
        }
        if head.starts_with(b"OggS") {
            return Container::Ogg;
        }
        if head.starts_with(b"RIFF") && head.len() >= 12 {
            return match &head[8..12] {
                b"WAVE" => Container::Wav,
                b"AVI " => Container::Avi,
                b"WEBP" => Container::Webp,
                _ => Container::Unknown,
            };
        }
        if head.starts_with(b"FORM") && head.len() >= 12 {
            // AIFF and AIFF-C share the IFF envelope.
            if &head[8..12] == b"AIFF" || &head[8..12] == b"AIFC" {
                return Container::Aiff;
            }
        }
        // ISO base media: a `ftyp` box at offset 4. Its size field
        // comes first, which is why the magic is not at offset 0.
        if head.len() >= 12 && &head[4..8] == b"ftyp" {
            return Container::Bmff;
        }
        // A bare MPEG audio frame sync, for MP3s with no ID3 tag.
        if head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0 {
            return Container::Mp3;
        }
        Container::Unknown
    }

    /// Whether this container can carry audio tags — an ID3 block,
    /// Vorbis comments, an MP4 `ilst`, a RIFF `INFO` list.
    ///
    /// Used to decide whether to *attempt* the tag reader, not what to
    /// do with the answer. Asking is cheap but not free (an open, a
    /// parse, and a log line per failure), and a photo library would
    /// otherwise pay it once per JPEG for a guaranteed miss.
    pub fn may_have_tags(self) -> bool {
        matches!(
            self,
            Container::Mp3
                | Container::Flac
                | Container::Wav
                | Container::Aiff
                | Container::Bmff
                | Container::Ogg
        )
    }

    /// Whether this container can embed an EXIF/TIFF block. `Bmff`
    /// appears in both lists, which is the point: an MP4 can carry
    /// `ilst` tags *and* capture metadata, and a file that has both
    /// should not lose one of them.
    pub fn may_have_exif(self) -> bool {
        matches!(
            self,
            Container::Jpeg | Container::Png | Container::Tiff | Container::Bmff | Container::Webp
        )
    }

    /// Class when the container alone settles it. `Bmff` and `Riff`
    /// do not — an `.m4a` and an `.mp4` are the same container — so
    /// those come back `None` and the caller falls back to the
    /// extension.
    pub fn implied_class(self) -> Option<MediaClass> {
        Some(match self {
            Container::Mp3 | Container::Flac | Container::Wav | Container::Aiff => {
                MediaClass::Audio
            }
            Container::Jpeg | Container::Png | Container::Tiff | Container::Gif => {
                MediaClass::Image
            }
            Container::Avi | Container::Matroska => MediaClass::Video,
            // WebP is usually a still but has an animated form; Ogg
            // carries audio far more often than video, but carries
            // both. Both defer to the extension.
            Container::Webp | Container::Ogg | Container::Bmff | Container::Unknown => return None,
        })
    }
}

/// How many leading bytes [`Container::sniff`] needs. The longest test
/// is the RIFF form type at offset 8..12.
pub const SNIFF_LEN: usize = 16;

/// Extensions we index, and the class we assume for each before reading
/// a byte. The bytes get the final say on `container`; this table gets
/// the final say on `class` for the containers that carry either.
///
/// Kept lowercase; [`class_for_extension`] lowercases before lookup.
const EXTENSIONS: &[(&str, MediaClass)] = &[
    // ── audio ────────────────────────────────────────────────────────
    ("mp3", MediaClass::Audio),
    ("flac", MediaClass::Audio),
    ("wav", MediaClass::Audio),
    ("wave", MediaClass::Audio),
    ("aif", MediaClass::Audio),
    ("aiff", MediaClass::Audio),
    ("aifc", MediaClass::Audio),
    ("m4a", MediaClass::Audio),
    ("m4b", MediaClass::Audio),
    ("aac", MediaClass::Audio),
    ("alac", MediaClass::Audio),
    ("ogg", MediaClass::Audio),
    ("oga", MediaClass::Audio),
    ("opus", MediaClass::Audio),
    ("wma", MediaClass::Audio),
    // ── image ────────────────────────────────────────────────────────
    ("jpg", MediaClass::Image),
    ("jpeg", MediaClass::Image),
    ("jpe", MediaClass::Image),
    ("png", MediaClass::Image),
    ("gif", MediaClass::Image),
    ("tif", MediaClass::Image),
    ("tiff", MediaClass::Image),
    ("dng", MediaClass::Image),
    ("cr2", MediaClass::Image),
    ("cr3", MediaClass::Image),
    ("nef", MediaClass::Image),
    ("arw", MediaClass::Image),
    ("orf", MediaClass::Image),
    ("raf", MediaClass::Image),
    ("rw2", MediaClass::Image),
    ("heic", MediaClass::Image),
    ("heif", MediaClass::Image),
    ("avif", MediaClass::Image),
    ("webp", MediaClass::Image),
    ("bmp", MediaClass::Image),
    // ── video ────────────────────────────────────────────────────────
    ("mp4", MediaClass::Video),
    ("m4v", MediaClass::Video),
    ("mov", MediaClass::Video),
    ("avi", MediaClass::Video),
    ("mkv", MediaClass::Video),
    ("webm", MediaClass::Video),
    ("wmv", MediaClass::Video),
    ("mpg", MediaClass::Video),
    ("mpeg", MediaClass::Video),
    ("m2ts", MediaClass::Video),
    ("mts", MediaClass::Video),
    ("3gp", MediaClass::Video),
];

/// Playlist extensions. Handled by a separate pass — see
/// `super::playlist`.
const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8"];

fn lower_ext(p: &Path) -> Option<String> {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The class implied by a path's extension, or `None` if we do not
/// index that extension at all.
pub fn class_for_extension(p: &Path) -> Option<MediaClass> {
    let ext = lower_ext(p)?;
    EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, class)| *class)
}

pub fn is_playlist_extension(p: &Path) -> bool {
    lower_ext(p).is_some_and(|e| PLAYLIST_EXTENSIONS.contains(&e.as_str()))
}

/// The walk predicate: a media file or a playlist.
///
/// Deliberately extension-only. This runs once per entry in a tree that
/// may hold millions of them, and the alternative — opening every file
/// to sniff it — is exactly the cost the Unison cursor exists to avoid.
pub fn accept(p: &Path) -> bool {
    class_for_extension(p).is_some() || is_playlist_extension(p)
}

/// The recorded class for one file: the container decides when it can,
/// the extension when it cannot.
pub fn resolve_class(container: Container, path: &Path) -> MediaClass {
    container
        .implied_class()
        .or_else(|| class_for_extension(path))
        // Reached only for an indexed extension whose bytes we could
        // not place *and* whose extension is not in the table, which
        // `accept` already excludes. Image is the least surprising
        // fallback for a still-dominated corpus.
        .unwrap_or(MediaClass::Image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(class_for_extension(&p("a.JPG")), Some(MediaClass::Image));
        assert_eq!(class_for_extension(&p("a.Mp3")), Some(MediaClass::Audio));
        assert_eq!(class_for_extension(&p("a.MOV")), Some(MediaClass::Video));
        assert_eq!(class_for_extension(&p("a.txt")), None);
        assert_eq!(class_for_extension(&p("mp3")), None);
    }

    #[test]
    fn accept_covers_media_and_playlists_only() {
        assert!(accept(&p("song.mp3")));
        assert!(accept(&p("list.m3u8")));
        assert!(!accept(&p("notes.txt")));
        assert!(!accept(&p("scan.pdf")));
    }

    #[test]
    fn sniffing_beats_the_extension() {
        // A PNG named `.jpg` is recorded as a PNG.
        assert_eq!(
            Container::sniff(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d"),
            Container::Png
        );
        assert_eq!(
            Container::sniff(b"\xff\xd8\xff\xe0\x00\x10JFIF"),
            Container::Jpeg
        );
        assert_eq!(
            Container::sniff(b"ID3\x04\x00\x00\x00\x00\x00\x00"),
            Container::Mp3
        );
        assert_eq!(Container::sniff(b"fLaC\x00\x00\x00\x22"), Container::Flac);
        assert_eq!(
            Container::sniff(b"II\x2a\x00\x08\x00\x00\x00"),
            Container::Tiff
        );
        assert_eq!(
            Container::sniff(b"MM\x00\x2a\x00\x00\x00\x08"),
            Container::Tiff
        );
    }

    #[test]
    fn riff_form_type_separates_wav_avi_webp() {
        assert_eq!(
            Container::sniff(b"RIFF\x00\x00\x00\x00WAVE"),
            Container::Wav
        );
        assert_eq!(
            Container::sniff(b"RIFF\x00\x00\x00\x00AVI "),
            Container::Avi
        );
        assert_eq!(
            Container::sniff(b"RIFF\x00\x00\x00\x00WEBP"),
            Container::Webp
        );
        assert_eq!(
            Container::sniff(b"RIFF\x00\x00\x00\x00XXXX"),
            Container::Unknown
        );
    }

    #[test]
    fn bmff_is_recognized_by_the_ftyp_box_not_offset_zero() {
        assert_eq!(
            Container::sniff(b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00"),
            Container::Bmff
        );
        assert_eq!(
            Container::sniff(b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00"),
            Container::Bmff
        );
    }

    #[test]
    fn bmff_class_comes_from_the_extension() {
        // Same container, three intents.
        assert_eq!(
            resolve_class(Container::Bmff, &p("s.m4a")),
            MediaClass::Audio
        );
        assert_eq!(
            resolve_class(Container::Bmff, &p("s.mp4")),
            MediaClass::Video
        );
        assert_eq!(
            resolve_class(Container::Bmff, &p("s.heic")),
            MediaClass::Image
        );
    }

    #[test]
    fn sniffed_container_overrides_a_lying_extension_for_class_too() {
        // `.jpg` holding PNG bytes: the container settles the class,
        // and both agree here — the point is that the container is
        // consulted first.
        assert_eq!(
            resolve_class(Container::Png, &p("a.jpg")),
            MediaClass::Image
        );
        // `.mp4` holding an AVI: container wins, still video.
        assert_eq!(
            resolve_class(Container::Avi, &p("a.mp4")),
            MediaClass::Video
        );
        // `.png` holding MP3 bytes: audio, despite the name.
        assert_eq!(
            resolve_class(Container::Mp3, &p("a.png")),
            MediaClass::Audio
        );
    }

    #[test]
    fn short_and_empty_heads_do_not_panic() {
        assert_eq!(Container::sniff(b""), Container::Unknown);
        assert_eq!(Container::sniff(b"R"), Container::Unknown);
        assert_eq!(Container::sniff(b"RIFF"), Container::Unknown);
        assert_eq!(
            Container::sniff(b"\x00\x00\x00\x20ftyp"),
            Container::Unknown
        );
    }

    /// BMFF is in both lists on purpose: a music video carries `ilst`
    /// tags and capture metadata, and reading only one loses the other.
    #[test]
    fn bmff_can_carry_both_kinds_of_metadata() {
        assert!(Container::Bmff.may_have_tags());
        assert!(Container::Bmff.may_have_exif());
        // …and the single-purpose containers are in exactly one.
        assert!(Container::Mp3.may_have_tags() && !Container::Mp3.may_have_exif());
        assert!(Container::Jpeg.may_have_exif() && !Container::Jpeg.may_have_tags());
        assert!(Container::Tiff.may_have_exif() && !Container::Tiff.may_have_tags());
        // Nothing is attempted for a container we could not identify.
        assert!(!Container::Unknown.may_have_tags());
        assert!(!Container::Unknown.may_have_exif());
    }

    #[test]
    fn class_strings_are_the_stored_values() {
        // These land in `media_items.media_class`; changing one is a
        // schema change.
        assert_eq!(MediaClass::Audio.as_str(), "audio");
        assert_eq!(MediaClass::Image.as_str(), "image");
        assert_eq!(MediaClass::Video.as_str(), "video");
    }
}
