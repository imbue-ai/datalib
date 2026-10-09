//! A Takeout left as Google sent it: the `.zip` or `.tgz` parts of one
//! export in one folder. Only when the parts, or the feeds that are on,
//! changed since the last run that finished are the files those feeds read
//! unpacked into a temporary directory, walked like an unpacked export and
//! deleted. INGEST.md § "Zipped exports" has the rules.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::stop::StopFlag;
use datalib_etl_files::fsscan::{self, FileScanCursor, ScannedFile};

use super::SyncFlags;

/// The parts each feed last finished reading, keyed `<feed>/<part file name>`.
pub const SCOPE: &str = "google_takeout/archives";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Tgz,
}

const EXTENSIONS: &[(&str, Kind)] = &[
    (".zip", Kind::Zip),
    (".tgz", Kind::Tgz),
    (".tar.gz", Kind::Tgz),
];

pub fn kind_of(name: &str) -> Option<Kind> {
    let lower = name.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|&(_, kind)| kind)
}

/// The export a part belongs to. Google names the parts
/// `takeout-<stamp>-<n>-<nnn>.zip`, and older exports
/// `takeout-<stamp>-<nnn>.zip`: the name less its numbered tail.
fn export_of(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    let ext = EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map_or(0, |(ext, _)| ext.len());
    let mut stem = &name[..name.len() - ext];
    while let Some((head, tail)) = stem.rsplit_once('-') {
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        stem = head;
    }
    stem
}

/// The archive parts among a folder's file names, sorted; empty when there
/// are none. Parts of two exports would be two snapshots read as one, so
/// that is refused. A dot-file is skipped: macOS leaves a `._<name>` twin
/// of every file it copies to a drive that cannot hold its metadata.
pub fn parts_of_one_export(names: &[String]) -> Result<Vec<String>> {
    let mut parts: Vec<String> = names
        .iter()
        .filter(|n| !n.starts_with('.') && kind_of(n).is_some())
        .cloned()
        .collect();
    parts.sort();
    let exports: BTreeSet<&str> = parts.iter().map(|n| export_of(n)).collect();
    if exports.len() > 1 {
        bail!(
            "the folder holds the parts of {} Takeout exports ({}); keep one export per folder",
            exports.len(),
            exports.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(parts)
}

/// The archive parts directly in `dir`, or none when it is not a folder.
pub fn parts_in(dir: &Path) -> Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        if entry.file_type()?.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    parts_of_one_export(&names)
}

/// A feed, as far as unpacking goes: the switch that turns it on and the
/// folder under `Takeout/` it reads.
struct Feed {
    name: &'static str,
    on: fn(&SyncFlags) -> bool,
    dir: &'static str,
}

const FEEDS: &[Feed] = &[
    Feed {
        name: "maps_reviews",
        on: |s| s.maps_reviews,
        dir: "Maps (your places)",
    },
    Feed {
        name: "maps_saved_places",
        on: |s| s.maps_saved_places,
        dir: "Maps (your places)",
    },
    Feed {
        name: "maps_photos",
        on: |s| s.maps_photos,
        dir: super::maps_photos::DIR_REL,
    },
    Feed {
        name: "youtube_watch_history",
        on: |s| s.youtube_watch_history,
        dir: "YouTube and YouTube Music/history",
    },
    Feed {
        name: "youtube_subscriptions",
        on: |s| s.youtube_subscriptions,
        dir: "YouTube and YouTube Music/subscriptions",
    },
    Feed {
        name: "google_chat",
        on: |s| s.google_chat,
        dir: "Google Chat",
    },
    Feed {
        name: "gemini_apps",
        on: |s| s.gemini_apps,
        dir: "My Activity/Gemini Apps",
    },
    Feed {
        name: "google_voice",
        on: |s| s.google_voice,
        dir: "Voice",
    },
    Feed {
        name: "google_voice_include_spam",
        on: |s| s.google_voice && s.google_voice_include_spam,
        dir: "Voice/Spam",
    },
];

fn feeds_on(sync: &SyncFlags) -> impl Iterator<Item = &'static Feed> + '_ {
    FEEDS.iter().filter(move |f| (f.on)(sync))
}

/// Whether a feed that is on reads the file at `rel` (under `Takeout/`).
/// The deepest folder holding it decides, so `Voice/Spam` stays packed
/// while Voice is on and its spam is not.
pub fn admits(sync: &SyncFlags, rel: &str) -> bool {
    let holding: Vec<&Feed> = FEEDS
        .iter()
        .filter(|f| fsscan::is_under(rel, f.dir))
        .collect();
    let Some(deepest) = holding.iter().map(|f| f.dir.len()).max() else {
        return false;
    };
    holding
        .iter()
        .any(|f| f.dir.len() == deepest && (f.on)(sync))
}

/// What a run that finishes stamps: each part, once per feed that is on.
pub fn stamps(sync: &SyncFlags, parts: &[ScannedFile]) -> Vec<ScannedFile> {
    feeds_on(sync)
        .flat_map(|feed| {
            parts.iter().map(move |p| ScannedFile {
                rel: format!("{}/{}", feed.name, p.rel),
                ..p.clone()
            })
        })
        .collect()
}

/// Whether the feeds that are on last finished reading exactly these parts.
/// A feed turned off since does not count; one turned on has no stamps yet.
pub fn unchanged(sync: &SyncFlags, prev: &FileScanCursor, stamps: &[ScannedFile]) -> bool {
    let on: BTreeSet<&str> = feeds_on(sync).map(|f| f.name).collect();
    let was: FileScanCursor = prev
        .iter()
        .filter(|(rel, _)| {
            rel.split_once('/')
                .is_some_and(|(feed, _)| on.contains(feed))
        })
        .map(|(rel, hash)| (rel.clone(), *hash))
        .collect();
    let now: FileScanCursor = stamps.iter().map(|s| (s.rel.clone(), s.blake3)).collect();
    was == now
}

/// Where an archive entry lands under the unpacked `Takeout/`, or `None`
/// for anything outside `Takeout/` or a name that climbs out of it.
pub fn entry_rel(name: &str) -> Option<String> {
    let rest = name
        .strip_prefix("./")
        .unwrap_or(name)
        .strip_prefix("Takeout/")?;
    let mut parts = Vec::new();
    for c in Path::new(rest).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub const STOPPED: &str = "stopped while unpacking the export; nothing was read";

/// Unpack the files a feed that is on reads from `parts` into `dest`, the
/// `Takeout/` folder to walk, and say how many came out.
pub fn unpack(parts: &[PathBuf], sync: &SyncFlags, dest: &Path, stop: &StopFlag) -> Result<usize> {
    let mut out = Unpacker {
        sync,
        dest,
        stop,
        files: 0,
    };
    for part in parts {
        let name = part.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let file = File::open(part).with_context(|| format!("open {}", part.display()))?;
        match kind_of(name) {
            Some(Kind::Zip) => out.zip(part, file)?,
            Some(Kind::Tgz) => out.tgz(part, file)?,
            None => bail!("{} is not a .zip or a .tgz", part.display()),
        }
    }
    Ok(out.files)
}

struct Unpacker<'a> {
    sync: &'a SyncFlags,
    dest: &'a Path,
    stop: &'a StopFlag,
    files: usize,
}

impl Unpacker<'_> {
    fn zip(&mut self, part: &Path, file: File) -> Result<()> {
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .with_context(|| format!("{} is not a readable zip", part.display()))?;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .with_context(|| format!("read entry {i} of {}", part.display()))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            self.entry(part, &name, &mut entry)?;
        }
        Ok(())
    }

    fn tgz(&mut self, part: &Path, file: File) -> Result<()> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
        let entries = archive
            .entries()
            .with_context(|| format!("{} is not a readable .tgz", part.display()))?;
        for entry in entries {
            let mut entry = entry.with_context(|| format!("read {}", part.display()))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = entry
                .path()
                .with_context(|| format!("read an entry's name in {}", part.display()))?
                .to_string_lossy()
                .into_owned();
            self.entry(part, &name, &mut entry)?;
        }
        Ok(())
    }

    fn entry(&mut self, part: &Path, name: &str, bytes: &mut dyn Read) -> Result<()> {
        if self.stop.requested() {
            bail!(STOPPED);
        }
        let Some(rel) = entry_rel(name).filter(|rel| admits(self.sync, rel)) else {
            return Ok(());
        };
        let path = self.dest.join(&rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut out = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        std::io::copy(bytes, &mut out)
            .with_context(|| format!("unpack {name} from {}", part.display()))?;
        self.files += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parts_are_grouped_by_the_export_google_named_them_for() {
        assert_eq!(
            export_of("takeout-23640301T090000Z-1-001.zip"),
            "takeout-23640301T090000Z"
        );
        assert_eq!(
            export_of("takeout-23630101T000000Z-003.tgz"),
            "takeout-23630101T000000Z"
        );
        assert_eq!(
            export_of("Takeout-23630101T000000Z-1-001.TAR.GZ"),
            "Takeout-23630101T000000Z"
        );
        let parts = parts_of_one_export(&names(&[
            "takeout-23640301T090000Z-2-001.zip",
            "takeout-23640301T090000Z-1-001.zip",
            "._takeout-23640301T090000Z-1-001.zip",
            "notes.txt",
        ]))
        .unwrap();
        assert_eq!(
            parts,
            names(&[
                "takeout-23640301T090000Z-1-001.zip",
                "takeout-23640301T090000Z-2-001.zip"
            ])
        );
        assert!(parts_of_one_export(&names(&["notes.txt"]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parts_of_two_exports_are_refused() {
        let err = parts_of_one_export(&names(&[
            "takeout-23640301T090000Z-1-001.zip",
            "takeout-23631231T000000Z-1-001.zip",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("2 Takeout exports"), "{err}");
    }

    #[test]
    fn only_entries_under_takeout_that_stay_inside_it_land() {
        assert_eq!(
            entry_rel("Takeout/Google Chat/Groups/x.json").as_deref(),
            Some("Google Chat/Groups/x.json")
        );
        assert_eq!(
            entry_rel("./Takeout/Voice/Bills.html").as_deref(),
            Some("Voice/Bills.html")
        );
        assert_eq!(entry_rel("Takeout/Voice/../../../escaped"), None);
        assert_eq!(entry_rel("Takeout//etc/passwd"), None);
        assert_eq!(entry_rel("Other/Voice/Bills.html"), None);
        assert_eq!(entry_rel("Takeout/"), None);
    }

    /// Every file a feed reads is unpacked when that feed is on, so a
    /// feed's folder renamed in one place and not the other fails here.
    #[test]
    fn each_feed_unpacks_the_files_it_reads() {
        let all = SyncFlags::all();
        for rel in [
            super::super::maps_reviews::FILE_REL,
            super::super::maps_saved_places::FILE_REL,
            &format!("{}/x.jpg", super::super::maps_photos::DIR_REL),
            super::super::youtube_watch_history::FILE_REL,
            super::super::youtube_subscriptions::FILE_REL,
            super::super::gemini_apps::FILE_REL,
            "Google Chat/Groups/DM X/messages.json",
            "Voice/Calls/x.html",
            "Voice/Spam/x.html",
            "Voice/Bills.html",
        ] {
            assert!(admits(&all, rel), "{rel}");
        }
        assert!(!admits(&all, "Drive/big.mp4"));
        assert!(!admits(&all, "YouTube and YouTube Music/videos/upload.mp4"));
        assert!(!admits(
            &SyncFlags::default(),
            "Google Chat/Groups/DM X/messages.json"
        ));
    }

    #[test]
    fn voice_spam_stays_packed_unless_it_is_asked_for() {
        let voice = SyncFlags {
            google_voice: true,
            ..SyncFlags::default()
        };
        assert!(admits(&voice, "Voice/Calls/x.html"));
        assert!(!admits(&voice, "Voice/Spam/x.html"));
    }

    fn part(rel: &str, byte: u8) -> ScannedFile {
        ScannedFile {
            path: PathBuf::from(rel),
            rel: rel.to_string(),
            size: 1,
            blake3: [byte; 32],
        }
    }

    fn cursor(stamps: &[ScannedFile]) -> FileScanCursor {
        stamps.iter().map(|s| (s.rel.clone(), s.blake3)).collect()
    }

    #[test]
    fn the_same_parts_and_feeds_are_unchanged() {
        let chat = SyncFlags {
            google_chat: true,
            ..SyncFlags::default()
        };
        let parts = [part("a.zip", 1), part("b.zip", 2)];
        let prev = cursor(&stamps(&chat, &parts));
        assert!(unchanged(&chat, &prev, &stamps(&chat, &parts)));

        let rewritten = [part("a.zip", 1), part("b.zip", 3)];
        assert!(!unchanged(&chat, &prev, &stamps(&chat, &rewritten)));
        let one_gone = [part("a.zip", 1)];
        assert!(!unchanged(&chat, &prev, &stamps(&chat, &one_gone)));

        let chat_and_voice = SyncFlags {
            google_voice: true,
            ..chat.clone()
        };
        assert!(
            !unchanged(&chat_and_voice, &prev, &stamps(&chat_and_voice, &parts)),
            "a feed turned on has not read these parts"
        );
        let prev_both = cursor(&stamps(&chat_and_voice, &parts));
        assert!(
            unchanged(&chat, &prev_both, &stamps(&chat, &parts)),
            "a feed turned off has nothing left to read"
        );
    }
}
