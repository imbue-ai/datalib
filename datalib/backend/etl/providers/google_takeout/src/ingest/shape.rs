//! What a problem row may say about a record of the export: its shape,
//! never its content. A Takeout file is often named for what it holds (a
//! note's title, a person, a phone number), and an entry's text or URL
//! is the account's, so a row names a file by [`shape_rel`], a URL by
//! [`url_kind`], and a value by its size.

/// Products whose folder and file names below them are Google's own
/// wording (`My Activity/Search/MyActivity.html`), never the account's.
const GOOGLE_WORDED: &[&str] = &[
    "My Activity",
    "Maps",
    "Maps (your places)",
    "Chrome",
    "Fit",
    "Google Account",
    "Profile",
    "Google Play Store",
    "Tasks",
    "Access Log Activity",
    "Google Pay",
    "Home App",
    "Location History (Timeline)",
    "Timeline",
    "Google Shopping",
    "Google Store",
    "Gemini",
    "Search Contributions",
    "Discover",
    "News",
];

/// Folder and file names Google uses in the products whose other names
/// are the account's: a Chat group's files, Voice's folders, YouTube's.
const GOOGLE_NAMES: &[&str] = &[
    "Groups",
    "Users",
    "messages.json",
    "group_info.json",
    "user_info.json",
    "Calls",
    "Spam",
    "Greetings",
    "Bills.html",
    "Phones.vcf",
    "history",
    "subscriptions",
    "playlists",
    "channels",
    "comments",
    "live chats",
    "video metadata",
    "watch-history.html",
    "watch-history.json",
    "search-history.html",
    "search-history.json",
    "subscriptions.csv",
    "playlists.csv",
    "comments.csv",
    "channel.csv",
    "videos.csv",
    "video texts.csv",
    "video recordings.csv",
    "Labels.txt",
    "metadata.json",
    "feed.atom",
    "Albums",
    "Blogs",
    "archive_browser.html",
];

const VOICE_KINDS: &[&str] = &[
    "Text",
    "Voicemail",
    "Missed",
    "Received",
    "Placed",
    "Recorded",
    "Group Conversation",
];

/// A path under `Takeout/` with every name that might be the account's
/// replaced by `<name>`: the product, Google's own names and the
/// extension stay. `Keep/Away team.json` → `Keep/<name>.json`.
pub fn shape_rel(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    let Some((product, rest)) = parts.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        return if GOOGLE_NAMES.contains(product) || is_worded(product) {
            product.to_string()
        } else {
            named(product, true)
        };
    }
    let worded = GOOGLE_WORDED.contains(product);
    let mut out = vec![product.to_string()];
    for (i, p) in rest.iter().enumerate() {
        let is_file = i == rest.len() - 1;
        out.push(shape_part(product, p, is_file, worded));
    }
    out.join("/")
}

fn shape_part(product: &str, p: &str, is_file: bool, worded: bool) -> String {
    if GOOGLE_NAMES.contains(&p) {
        return p.to_string();
    }
    if matches!(product, "Google Chat" | "Hangouts") && !is_file {
        for kind in ["DM", "Space", "User"] {
            if p.strip_prefix(kind)
                .is_some_and(|rest| rest.starts_with(' '))
            {
                return format!("{kind} <id>");
            }
        }
    }
    if product == "Voice" && is_file {
        if let Some(shaped) = voice_record(p) {
            return shaped;
        }
    }
    if worded {
        let (stem, ext) = p.rsplit_once('.').unwrap_or((p, ""));
        if !is_file && is_worded(p) {
            return p.to_string();
        }
        let data = matches!(
            ext.to_ascii_lowercase().as_str(),
            "json" | "html" | "csv" | "txt" | "xml" | "jsonl"
        );
        if is_file && data && is_worded(stem) {
            return p.to_string();
        }
    }
    named(p, is_file)
}

/// Letters, spaces and a little punctuation: Google's wording, which a
/// file the account named could also be, so only trusted where the
/// product's names are all Google's.
fn is_worded(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.len() <= 60
        && chars.all(|c| c.is_ascii_alphabetic() || " _&()'-".contains(c))
}

/// A file's name as [`shape_rel`] shows it: `<name>.png`.
pub fn shape_name(name: &str) -> String {
    named(name, true)
}

/// `<name>` keeping the extension, and a suffix Google adds to a name
/// (`-videos.csv`, a photo sidecar's `.jpg.json`).
fn named(p: &str, is_file: bool) -> String {
    if !is_file {
        return "<name>".to_string();
    }
    if p.ends_with("-videos.csv") {
        return "<name>-videos.csv".to_string();
    }
    let mut exts: Vec<String> = Vec::new();
    let mut rest = p;
    while exts.len() < 2 {
        let Some((stem, ext)) = rest.rsplit_once('.') else {
            break;
        };
        if stem.is_empty()
            || ext.is_empty()
            || ext.len() > 6
            || !ext.chars().all(|c| c.is_ascii_alphanumeric())
        {
            break;
        }
        let lower = ext.to_ascii_lowercase();
        // A second extension only where it is a media file's, as in a
        // sidecar named after its photo.
        if !exts.is_empty() && !is_media_ext(&lower) {
            break;
        }
        exts.insert(0, lower);
        rest = stem;
    }
    if exts.is_empty() {
        "<name>".to_string()
    } else {
        format!("<name>.{}", exts.join("."))
    }
}

fn is_media_ext(e: &str) -> bool {
    matches!(
        e,
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "webp" | "mp4" | "mov" | "mp3" | "dng"
    )
}

/// `<who> - Text - 2364-03-01T09_00_00Z-1-1.jpg` →
/// `<name> - Text - <stamp>-N-N.jpg`.
fn voice_record(p: &str) -> Option<String> {
    let (_, after) = VOICE_KINDS.iter().find_map(|k| {
        p.split_once(&format!(" - {k} - "))
            .map(|(a, b)| (a, (k, b)))
    })?;
    let (kind, tail) = after;
    let stamp_len = "2364-03-01T09_00_00Z".len();
    let rest = tail.get(stamp_len..)?;
    let (stem, ext) = match rest.rsplit_once('.') {
        Some((stem, ext)) if ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => {
            (stem, ext)
        }
        _ => (rest, ""),
    };
    let stem: String = stem
        .chars()
        .map(|c| if c.is_ascii_digit() { 'N' } else { c })
        .collect();
    let mut squeezed = String::new();
    for c in stem.chars() {
        if !(c == 'N' && squeezed.ends_with('N')) {
            squeezed.push(c);
        }
    }
    if !squeezed.chars().all(|c| c == 'N' || c == '-') {
        squeezed = "<…>".to_string();
    }
    let ext = ext.to_ascii_lowercase();
    let dot = if ext.is_empty() { "" } else { "." };
    Some(format!("<name> - {kind} - <stamp>{squeezed}{dot}{ext}"))
}

/// The host and first path segment of a URL, the segment only where it
/// is a short lowercase word: `www.youtube.com/post`, `maps.google.com/`.
pub fn url_kind(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.rsplit('@').next().unwrap_or(host);
    let seg = path.split(['/', '?', '#']).next().unwrap_or("");
    let seg = if !seg.is_empty() && seg.len() <= 20 && seg.chars().all(|c| c.is_ascii_lowercase()) {
        seg
    } else if seg.is_empty() {
        ""
    } else {
        "<…>"
    };
    format!("{host}/{seg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_keeps_googles_names_and_hides_the_accounts() {
        for (rel, shaped) in [
            ("Keep/Away team.json", "Keep/<name>.json"),
            ("Keep/Labels.txt", "Keep/Labels.txt"),
            (
                "Google Chat/Groups/DM TNG-BRIDGE/messages.json",
                "Google Chat/Groups/DM <id>/messages.json",
            ),
            (
                "Google Chat/Groups/Space AAAA/risa shore leave.png",
                "Google Chat/Groups/Space <id>/<name>.png",
            ),
            (
                "Google Chat/Users/User 1234567890/Picard notes.json",
                "Google Chat/Users/User <id>/<name>.json",
            ),
            (
                "My Activity/Search/MyActivity.html",
                "My Activity/Search/MyActivity.html",
            ),
            (
                "My Activity/Assistant/2364-01-01 log.mp3",
                "My Activity/Assistant/<name>.mp3",
            ),
            (
                "Maps/Photos and videos/ten forward.jpg.json",
                "Maps/Photos and videos/<name>.jpg.json",
            ),
            (
                "Google Account/jean.luc.SubscriberInfo.html",
                "Google Account/<name>.html",
            ),
            (
                "YouTube and YouTube Music/playlists/Klingon opera-videos.csv",
                "YouTube and YouTube Music/playlists/<name>-videos.csv",
            ),
            (
                "Voice/Calls/Jean-Luc Picard - Text - 2364-03-01T09_00_00Z-1-1.jpg",
                "Voice/Calls/<name> - Text - <stamp>-N-N.jpg",
            ),
            (
                "Voice/Calls/Worf - Voicemail - 2364-03-02T10_00_00Z.mp3",
                "Voice/Calls/<name> - Voicemail - <stamp>.mp3",
            ),
            (
                "Drive/Away team/duty roster 2364.docx",
                "Drive/<name>/<name>.docx",
            ),
            ("Saved/Starbases.csv", "Saved/<name>.csv"),
            ("archive_browser.html", "archive_browser.html"),
            ("Tasks/Tasks.json", "Tasks/Tasks.json"),
        ] {
            assert_eq!(shape_rel(rel), shaped, "{rel}");
        }
    }

    #[test]
    fn a_url_keeps_its_host_and_a_word() {
        assert_eq!(
            url_kind("https://www.youtube.com/post/UgkxTenForward"),
            "www.youtube.com/post"
        );
        assert_eq!(
            url_kind("https://maps.google.com/?q=Ten+Forward"),
            "maps.google.com/"
        );
        assert_eq!(
            url_kind("https://www.youtube.com/channel/UCpicard"),
            "www.youtube.com/channel"
        );
        assert_eq!(url_kind("https://goo.gl/Xy12"), "goo.gl/<…>");
    }
}
