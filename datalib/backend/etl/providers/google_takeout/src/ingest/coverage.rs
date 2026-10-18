//! Which feed reads each file of the export, and a problem row for each
//! kind of file none does, so a run over an export says what it left out.
//! A row names its files by the shape of their path ([`shape_rel`]),
//! never by their names: in Takeout a name is often what the file says
//! (a note's title, a person, a phone number).

use std::collections::BTreeMap;

use datalib_etl::download_problems::SkippedRecord;
use datalib_etl_files::fsscan;
use datalib_problems::{Problem, Reason, Severity};

use super::shape::shape_rel;
use super::{
    maps_photos, maps_reviews, maps_saved_places, youtube_subscriptions, youtube_watch_history,
    SyncFlags,
};

/// The `skipped:` part the rows are filed under, replaced whole each run
/// that reads the export.
pub const PART: &str = "unread_files";

/// The feed that reads the file at `rel` (under `Takeout/`), whether or
/// not it is on.
pub fn feed_of(rel: &str) -> Option<&'static str> {
    let parts: Vec<&str> = rel.split('/').collect();
    let under = |dir: &str| fsscan::is_under(rel, dir);
    if rel == maps_reviews::FILE_REL {
        Some("maps_reviews")
    } else if rel == maps_saved_places::FILE_REL {
        Some("maps_saved_places")
    } else if under(maps_photos::DIR_REL) {
        Some("maps_photos")
    } else if rel == youtube_watch_history::FILE_REL {
        Some("youtube_watch_history")
    } else if rel == youtube_subscriptions::FILE_REL {
        Some("youtube_subscriptions")
    } else if matches!(
        parts.as_slice(),
        ["Google Chat", "Users", _, "user_info.json"]
    ) || matches!(parts.as_slice(), ["Google Chat", "Groups", _, _])
    {
        Some("google_chat")
    } else if under("My Activity/Gemini Apps") {
        Some("gemini_apps")
    } else if rel == "Voice/Bills.html" || under("Voice/Calls") || under("Voice/Greetings") {
        Some("google_voice")
    } else if under("Voice/Spam") {
        Some("google_voice_include_spam")
    } else {
        None
    }
}

fn is_on(sync: &SyncFlags, feed: &str) -> bool {
    match feed {
        "maps_reviews" => sync.maps_reviews,
        "maps_saved_places" => sync.maps_saved_places,
        "maps_photos" => sync.maps_photos,
        "youtube_watch_history" => sync.youtube_watch_history,
        "youtube_subscriptions" => sync.youtube_subscriptions,
        "google_chat" => sync.google_chat,
        "gemini_apps" => sync.gemini_apps,
        "google_voice" => sync.google_voice,
        "google_voice_include_spam" => sync.google_voice && sync.google_voice_include_spam,
        _ => false,
    }
}

/// What the export holds that this source leaves out on purpose, and why:
/// another source reads it, or it is not something a person wrote.
fn left_out(rel: &str) -> Option<&'static str> {
    if rel == "archive_browser.html" {
        return Some("Takeout's own index of the export");
    }
    if rel == "Voice/Phones.vcf" {
        return Some("the account's own phone numbers");
    }
    let product = rel.split('/').next().unwrap_or("");
    Some(match product {
        "Mail" | "Gmail" => "email, which the email source reads",
        "Calendar" => "calendars, which the calendar source reads",
        "Contacts" => "contacts, which the contacts source reads",
        "Chrome" => "browser history, bookmarks, settings and autofill",
        "Google Pay" | "Google Wallet" => "payments",
        "Fit" | "Fitbit" | "Google Health" => "health measurements",
        "Location History" | "Location History (Timeline)" | "Timeline" => "where the phone was",
        "Nest" | "Home App" => "home devices",
        "Google Account"
        | "Profile"
        | "Access Log Activity"
        | "Android Device Configuration Service" => "account settings",
        _ => return None,
    })
}

/// A row for each shape of file no feed that is on reads: a warning where
/// no feed reads it at all, a note where one is off or the source leaves
/// it out on purpose.
pub fn report<'a>(
    files: impl IntoIterator<Item = (&'a str, i64)>,
    sync: &SyncFlags,
) -> Vec<SkippedRecord> {
    #[derive(Default)]
    struct Group {
        files: usize,
        bytes: i64,
    }
    let mut groups: BTreeMap<(String, Why), Group> = BTreeMap::new();
    for (rel, size) in files {
        if rel.split('/').any(|p| p.starts_with('.')) {
            continue;
        }
        let why = match feed_of(rel) {
            Some(feed) if is_on(sync, feed) => continue,
            Some(feed) => Why::Off(feed),
            None => match left_out(rel) {
                Some(why) => Why::LeftOut(why),
                None => Why::NoFeed,
            },
        };
        let g = groups.entry((shape_rel(rel), why)).or_default();
        g.files += 1;
        g.bytes += size.max(0);
    }
    groups
        .into_iter()
        .map(|((shape, why), g)| {
            let what = format!("{shape}: {} ({})", files_word(g.files), bytes_word(g.bytes));
            let problem = match why {
                Why::NoFeed => Problem::explained(
                    Reason::UncoveredType,
                    Some("file".to_string()),
                    &format!("{what} that no feed reads"),
                )
                .severity(Severity::Warning),
                Why::Off(feed) => Problem::explained(
                    Reason::Noted,
                    Some("file".to_string()),
                    &format!("{what} not read: the {feed} feed is off"),
                )
                .severity(Severity::Info),
                Why::LeftOut(why) => Problem::explained(
                    Reason::Noted,
                    Some("file".to_string()),
                    &format!("{what} left out on purpose: {why}"),
                )
                .severity(Severity::Info),
            };
            SkippedRecord {
                entry: shape,
                problem,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    NoFeed,
    Off(&'static str),
    LeftOut(&'static str),
}

fn files_word(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

fn bytes_word(b: i64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if b < 1000 {
        return format!("{b} bytes");
    }
    let mut v = b as f64;
    let mut unit = "bytes";
    for u in UNITS {
        if v < 1000.0 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_a_feed_reads_is_claimed() {
        for (rel, feed) in [
            (maps_reviews::FILE_REL, "maps_reviews"),
            ("Maps/Photos and videos/x.jpg", "maps_photos"),
            ("Google Chat/Groups/DM X/x.png", "google_chat"),
            ("Google Chat/Users/User 1/user_info.json", "google_chat"),
            ("My Activity/Gemini Apps/MyActivity.html", "gemini_apps"),
            (
                "Voice/Calls/x - Text - 2364-03-01T09_00_00Z.html",
                "google_voice",
            ),
            ("Voice/Spam/x.html", "google_voice_include_spam"),
        ] {
            assert_eq!(feed_of(rel), Some(feed), "{rel}");
        }
        assert_eq!(feed_of("Google Chat/Users/User 1/other.json"), None);
        assert_eq!(
            feed_of("YouTube and YouTube Music/history/search-history.html"),
            None
        );
    }

    /// A file no feed reads is a warning, one a feed that is off reads a
    /// note, one left out on purpose a note; a file read is nothing.
    #[test]
    fn rows_say_why_a_kind_of_file_was_not_read() {
        let sync = SyncFlags {
            google_chat: true,
            ..SyncFlags::default()
        };
        let rows = report(
            [
                ("Keep/Away team.json", 2_000),
                ("Keep/Shore leave.json", 1_000),
                ("Google Chat/Groups/DM X/messages.json", 10),
                ("Voice/Bills.html", 5),
                ("Chrome/History.json", 7),
                ("Keep/.DS_Store", 7),
            ],
            &sync,
        );
        let got: Vec<(String, Option<Severity>, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.entry.clone(),
                    r.problem.severity,
                    r.problem.sample.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "Chrome/History.json".to_string(),
                    Some(Severity::Info),
                    "Chrome/History.json: 1 file (7 bytes) left out on purpose: browser history, bookmarks, settings and autofill".to_string()
                ),
                (
                    "Keep/<name>.json".to_string(),
                    Some(Severity::Warning),
                    "Keep/<name>.json: 2 files (3.0 KB) that no feed reads".to_string()
                ),
                (
                    "Voice/Bills.html".to_string(),
                    Some(Severity::Info),
                    "Voice/Bills.html: 1 file (5 bytes) not read: the google_voice feed is off".to_string()
                ),
            ]
        );
    }
}
