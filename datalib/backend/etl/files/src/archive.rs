//! An export left as it was sent: the `.zip` or `.tgz` parts of one
//! export, read entry by entry without unpacking them to disk. Google
//! Takeout is the shape this is written for: one export is one or more
//! numbered parts in one folder.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::expansion::Allowance;

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
            "the folder holds the parts of {} exports ({}); keep one export per folder",
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

/// An entry's name as a relative path that stays inside wherever it is
/// unpacked to, slash-separated, or `None` for one that climbs out.
pub fn safe_rel(name: &str) -> Option<String> {
    let mut parts = Vec::new();
    for c in Path::new(name.strip_prefix("./").unwrap_or(name)).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// One file inside a part, as [`for_each_entry`] hands it over.
pub struct Entry<'a> {
    /// The name the archive gives it, as written.
    pub name: &'a str,
    pub size: u64,
    pub bytes: &'a mut dyn Read,
}

/// Every file in `part`, in the order the archive holds them; folders and
/// links are passed over. An entry `visit` does not read is skipped
/// without being decompressed in a zip, and read past in a `.tgz`. A part
/// that expands too far ([`Allowance`]) fails the read that reaches the
/// limit.
pub fn for_each_entry(part: &Path, visit: impl FnMut(Entry<'_>) -> Result<()>) -> Result<()> {
    for_each_entry_within(part, &Allowance::of_file(part)?, visit)
}

fn for_each_entry_within(
    part: &Path,
    allowance: &Allowance,
    mut visit: impl FnMut(Entry<'_>) -> Result<()>,
) -> Result<()> {
    let name = part.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let file = File::open(part).with_context(|| format!("open {}", part.display()))?;
    match kind_of(name) {
        Some(Kind::Zip) => zip_entries(part, file, allowance, &mut visit),
        Some(Kind::Tgz) => tgz_entries(part, file, allowance, &mut visit),
        None => bail!("{} is not a .zip or a .tgz", part.display()),
    }
}

fn zip_entries(
    part: &Path,
    file: File,
    allowance: &Allowance,
    visit: &mut dyn FnMut(Entry<'_>) -> Result<()>,
) -> Result<()> {
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("{} is not a readable zip", part.display()))?;
    allowance.spend_entries(archive.len() as u64)?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("read entry {i} of {}", part.display()))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .with_context(|| format!("name of entry {i} in {}", part.display()))?
            .to_string();
        let size = entry.size();
        visit(Entry {
            name: &name,
            size,
            bytes: &mut allowance.meter(&mut entry),
        })?;
    }
    Ok(())
}

fn tgz_entries(
    part: &Path,
    file: File,
    allowance: &Allowance,
    visit: &mut dyn FnMut(Entry<'_>) -> Result<()>,
) -> Result<()> {
    // Metered under `tar`: headers and the entries read past count too, since
    // they cost the same to decompress.
    let gz = flate2::read::GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(allowance.meter(gz));
    let entries = archive
        .entries()
        .with_context(|| format!("{} is not a readable .tgz", part.display()))?;
    for entry in entries {
        let mut entry = entry.with_context(|| format!("read {}", part.display()))?;
        allowance.count_entry()?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .with_context(|| format!("read an entry's name in {}", part.display()))?
            .to_string_lossy()
            .into_owned();
        let size = entry.header().size().unwrap_or(0);
        visit(Entry {
            name: &name,
            size,
            bytes: &mut entry,
        })?;
    }
    Ok(())
}

/// What a configured path is when it may be an export as sent: the parts
/// to read when it is one `.zip`/`.tgz` or a folder of one export's
/// parts, or `None` when it is anything else (an unpacked folder, a
/// single file of another kind).
pub fn parts_at(path: &Path) -> Result<Option<Vec<PathBuf>>> {
    if path.is_file() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        return Ok(kind_of(name).map(|_| vec![path.to_path_buf()]));
    }
    let parts = parts_in(path)?;
    Ok((!parts.is_empty()).then(|| parts.iter().map(|p| path.join(p)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        assert!(err.contains("2 exports"), "{err}");
    }

    #[test]
    fn a_name_that_climbs_out_has_no_rel() {
        assert_eq!(
            safe_rel("Takeout/Mail/All mail.mbox").as_deref(),
            Some("Takeout/Mail/All mail.mbox")
        );
        assert_eq!(safe_rel("./Takeout/x").as_deref(), Some("Takeout/x"));
        assert_eq!(safe_rel("Takeout/../../escaped"), None);
        assert_eq!(safe_rel("/etc/passwd"), None);
        assert_eq!(safe_rel(""), None);
    }

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.add_directory("Takeout/", opts).unwrap();
        for (name, bytes) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
    }

    fn tgz_with(path: &Path, entries: &[(&str, &[u8])]) {
        let gz =
            flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        let mut w = tar::Builder::new(gz);
        for (name, bytes) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            w.append_data(&mut h, name, *bytes).unwrap();
        }
        w.into_inner().unwrap().finish().unwrap();
    }

    fn read_all(part: &Path) -> Vec<(String, u64, Vec<u8>)> {
        let mut out = Vec::new();
        for_each_entry(part, |e| {
            let mut bytes = Vec::new();
            e.bytes.read_to_end(&mut bytes)?;
            out.push((e.name.to_string(), e.size, bytes));
            Ok(())
        })
        .unwrap();
        out
    }

    /// Both kinds hand over the same files, folders left out.
    #[test]
    fn a_zip_and_a_tgz_yield_their_files() {
        let d = tempfile::tempdir().unwrap();
        let entries: &[(&str, &[u8])] = &[
            ("Takeout/Mail/All mail.mbox", b"From 1@xxx\n\nhi\n"),
            ("Takeout/archive_browser.html", b"<html/>"),
        ];
        let want: Vec<(String, u64, Vec<u8>)> = entries
            .iter()
            .map(|(n, b)| (n.to_string(), b.len() as u64, b.to_vec()))
            .collect();
        let zip = d.path().join("takeout-1-001.zip");
        zip_with(&zip, entries);
        assert_eq!(read_all(&zip), want);
        let tgz = d.path().join("takeout-1-001.tgz");
        tgz_with(&tgz, entries);
        assert_eq!(read_all(&tgz), want);
    }

    fn read_within(part: &Path, allowance: &Allowance) -> Result<usize> {
        let mut read = 0;
        for_each_entry_within(part, allowance, |e| {
            read += std::io::copy(e.bytes, &mut std::io::sink())? as usize;
            Ok(())
        })?;
        Ok(read)
    }

    /// A zip whose entries each stay under the limit still spends one
    /// allowance between them, and a `.tgz` the same.
    #[test]
    fn a_part_stops_where_it_expands_past_its_allowance() {
        let d = tempfile::tempdir().unwrap();
        let entries: &[(&str, &[u8])] = &[("a", &[0; 600]), ("b", &[0; 600])];
        let zip = d.path().join("bomb.zip");
        zip_with(&zip, entries);
        let tgz = d.path().join("bomb.tgz");
        tgz_with(&tgz, entries);
        for part in [&zip, &tgz] {
            let big_enough = Allowance::new(1 << 20, 10, part.display());
            assert_eq!(read_within(part, &big_enough).unwrap(), 1200);
            let too_small = Allowance::new(1000, 10, part.display());
            let e = format!("{:#}", read_within(part, &too_small).unwrap_err());
            assert!(e.contains("more than 1000 bytes"), "{e}");
        }
    }

    /// A `.tgz` entry nobody reads is still decompressed to get past it, so
    /// it spends the allowance; a zip's is skipped and does not.
    #[test]
    fn only_a_tgz_pays_for_an_entry_nobody_reads() {
        let d = tempfile::tempdir().unwrap();
        let entries: &[(&str, &[u8])] = &[("big", &[0; 1 << 16]), ("small", b"x")];
        let unread =
            |part: &Path, allowance: &Allowance| for_each_entry_within(part, allowance, |_| Ok(()));
        let zip = d.path().join("skip.zip");
        zip_with(&zip, entries);
        unread(&zip, &Allowance::new(1024, 10, "zip")).unwrap();
        let tgz = d.path().join("skip.tgz");
        tgz_with(&tgz, entries);
        let e = format!(
            "{:#}",
            unread(&tgz, &Allowance::new(1024, 10, "tgz")).unwrap_err()
        );
        assert!(e.contains("more than 1024 bytes"), "{e}");
    }

    #[test]
    fn a_part_of_too_many_entries_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let entries: &[(&str, &[u8])] = &[("a", b"x"), ("b", b"x"), ("c", b"x")];
        let zip = d.path().join("many.zip");
        zip_with(&zip, entries);
        let tgz = d.path().join("many.tgz");
        tgz_with(&tgz, entries);
        for part in [&zip, &tgz] {
            let few = Allowance::new(1 << 20, 2, part.display());
            let e = format!("{:#}", read_within(part, &few).unwrap_err());
            assert!(e.contains("too many entries"), "{e}");
        }
    }

    /// Through the public door, with the limits production runs under: a
    /// few hundred kilobytes that would come out at over 256 MiB.
    #[test]
    fn a_small_zip_bomb_is_refused_at_the_floor() {
        let d = tempfile::tempdir().unwrap();
        let zip = d.path().join("bomb.zip");
        let mut w = zip::ZipWriter::new(File::create(&zip).unwrap());
        w.start_file("All mail.mbox", zip::write::SimpleFileOptions::default())
            .unwrap();
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..=(datalib_etl::expansion::MIN_BYTES >> 20) {
            w.write_all(&chunk).unwrap();
        }
        w.finish().unwrap();
        assert!(std::fs::metadata(&zip).unwrap().len() < 2 << 20);

        let e = for_each_entry(&zip, |e| {
            std::io::copy(e.bytes, &mut std::io::sink())?;
            Ok(())
        })
        .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("too far for a real archive"), "{e}");
    }

    #[test]
    fn a_path_is_parts_only_when_it_holds_an_export_as_sent() {
        let d = tempfile::tempdir().unwrap();
        let zip = d.path().join("takeout-23640301T090000Z-1-001.zip");
        zip_with(&zip, &[]);
        assert_eq!(parts_at(&zip).unwrap(), Some(vec![zip.clone()]));
        assert_eq!(parts_at(d.path()).unwrap(), Some(vec![zip]));
        let mbox = d.path().join("All mail.mbox");
        std::fs::write(&mbox, "").unwrap();
        assert_eq!(parts_at(&mbox).unwrap(), None);
        let unpacked = d.path().join("Mail");
        std::fs::create_dir(&unpacked).unwrap();
        assert_eq!(parts_at(&unpacked).unwrap(), None);
    }
}
