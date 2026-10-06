//! A catalog file ready to mirror: a `.lrcat` as it is, or the one
//! `.lrcat` inside a Lightroom backup `.zip`, unpacked to a temporary
//! directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sqlx::sqlite::SqlitePool;

use datalib_etl::progress::Progress;
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};

pub struct Unpacked {
    /// Holds the unpacked copy; deleting it on drop is the cleanup.
    dir: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl Unpacked {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A private copy nothing else has open, so a snapshot of it buys
    /// nothing.
    pub fn is_copy(&self) -> bool {
        self.dir.is_some()
    }
}

pub fn is_zip(path: &Path) -> bool {
    has_extension(path, "zip")
}

pub fn is_catalog(path: &Path) -> bool {
    has_extension(path, "lrcat")
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

pub async fn unpack(path: &Path) -> Result<Unpacked> {
    if !is_zip(path) {
        return Ok(Unpacked {
            dir: None,
            path: path.to_path_buf(),
        });
    }
    let zip = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().context("create a directory to unpack the backup into")?;
        let path = extract_catalog(&zip, dir.path())?;
        Ok(Unpacked {
            dir: Some(dir),
            path,
        })
    })
    .await
    .context("unpack task panicked")?
}

pub async fn mirror_file(
    pool: &SqlitePool,
    file: &Path,
    options: &MirrorOptions,
    progress: &Progress,
) -> Result<MirrorStats> {
    let catalog = unpack(file).await?;
    let options = MirrorOptions {
        source_path: catalog.path().to_path_buf(),
        snapshot: options.snapshot && !catalog.is_copy(),
        ..options.clone()
    };
    mirror::run(pool, &options, progress).await
}

fn extract_catalog(zip_path: &Path, dest: &Path) -> Result<PathBuf> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("{} is not a readable zip", zip_path.display()))?;

    let mut catalogs: Vec<(usize, String)> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i)?;
        let name = entry.name();
        // Finder's zips carry a resource-fork twin of every file here.
        if entry.is_dir() || name.starts_with("__MACOSX/") {
            continue;
        }
        if is_catalog(Path::new(name)) {
            catalogs.push((i, name.to_string()));
        }
    }
    let (index, name) = match catalogs.as_slice() {
        [one] => one.clone(),
        [] => bail!("{} holds no .lrcat", zip_path.display()),
        many => bail!(
            "{} holds {} .lrcat files ({}); a backup holds one catalog",
            zip_path.display(),
            many.len(),
            many.iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    // Only the entry's last component is used, so a name like
    // `../../x.lrcat` cannot write outside `dest`.
    let file_name = Path::new(&name)
        .file_name()
        .with_context(|| format!("zip entry {name:?} has no file name"))?;
    let out_path = dest.join(file_name);
    let mut entry = archive.by_index(index)?;
    let mut out = std::fs::File::create(&out_path)
        .with_context(|| format!("create {}", out_path.display()))?;
    std::io::copy(&mut entry, &mut out)
        .with_context(|| format!("unpack {name} from {}", zip_path.display()))?;
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn unpacks_the_one_catalog_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("Lightroom Catalog-v13-3.zip");
        write_zip(
            &zip,
            &[
                ("__MACOSX/._Lightroom Catalog-v13-3.lrcat", b"fork"),
                ("Lightroom Catalog-v13-3.lrcat", b"SQLite format 3\0"),
            ],
        );
        let out = tempfile::tempdir().unwrap();
        let path = extract_catalog(&zip, out.path()).unwrap();
        assert_eq!(path, out.path().join("Lightroom Catalog-v13-3.lrcat"));
        assert_eq!(std::fs::read(&path).unwrap(), b"SQLite format 3\0");
    }

    #[test]
    fn an_entry_cannot_climb_out_of_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("b.zip");
        write_zip(&zip, &[("../../escaped.lrcat", b"x")]);
        let out = tempfile::tempdir().unwrap();
        let path = extract_catalog(&zip, out.path()).unwrap();
        assert_eq!(path, out.path().join("escaped.lrcat"));
    }

    #[test]
    fn a_zip_with_no_catalog_or_two_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let none = dir.path().join("none.zip");
        write_zip(&none, &[("notes.txt", b"x")]);
        assert!(extract_catalog(&none, out.path()).is_err());
        let two = dir.path().join("two.zip");
        write_zip(&two, &[("a.lrcat", b"x"), ("b.lrcat", b"y")]);
        let err = extract_catalog(&two, out.path()).unwrap_err().to_string();
        assert!(err.contains("2 .lrcat files"), "{err}");
    }
}
