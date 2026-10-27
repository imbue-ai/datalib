//! A ChatGPT data export as the export ingest reads it: the `.zip`
//! ChatGPT sends, read in place, or the folder it was unzipped into.
//! Either way a file is named relative to the export's root, the folder
//! holding `export_manifest.json` (or, in an older export,
//! `conversations.json`), so a zip of the unzipped folder reads the same.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

const MANIFEST: &str = "export_manifest.json";
const CONVERSATIONS: &str = "conversations.json";

pub struct ExportArchive {
    source: Source,
    /// `""`, or the folder inside the zip or the folder the root is,
    /// with a trailing `/`.
    root: String,
    /// Every file, `/`-separated and relative to `path`, in sorted order.
    names: Vec<String>,
    path: PathBuf,
}

enum Source {
    Dir,
    Zip(Box<zip::ZipArchive<BufReader<File>>>),
}

impl ExportArchive {
    pub fn open(path: &Path) -> Result<Self> {
        let (source, mut names) = if path.is_dir() {
            (Source::Dir, names_under(path)?)
        } else if path.is_file() {
            let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
            let zip = zip::ZipArchive::new(BufReader::new(file))
                .with_context(|| format!("{} is not a folder or a readable zip", path.display()))?;
            let names: Vec<String> = zip
                .file_names()
                .map(|n| n.map(|n| n.into_owned()))
                .collect::<Result<_, _>>()
                .with_context(|| format!("read the names in {}", path.display()))?;
            let files = names.into_iter().filter(|n| !n.ends_with('/')).collect();
            (Source::Zip(Box::new(zip)), files)
        } else {
            bail!("`export.path` {} does not exist", path.display());
        };
        names.sort();
        let Some(root) = root_of(&names) else {
            bail!(
                "no {MANIFEST} or {CONVERSATIONS} in {} — point `export.path` at the \
                 ChatGPT export's .zip, or the folder you unzipped it into",
                path.display()
            );
        };
        Ok(Self {
            source,
            root,
            names,
            path: path.to_path_buf(),
        })
    }

    /// The bytes of `name` under the root; `None` when there is no such file.
    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let full = format!("{}{name}", self.root);
        if self.names.binary_search(&full).is_err() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        match &mut self.source {
            Source::Dir => {
                bytes = std::fs::read(self.path.join(&full))
                    .with_context(|| format!("read {full} in {}", self.path.display()))?;
            }
            Source::Zip(zip) => {
                zip.by_name(&full)
                    .with_context(|| format!("find {full} in {}", self.path.display()))?
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("unzip {full} from {}", self.path.display()))?;
            }
        }
        Ok(Some(bytes))
    }

    pub fn read_json(&mut self, name: &str) -> Result<Option<Value>> {
        self.read(name)?
            .map(|b| serde_json::from_slice(&b).with_context(|| format!("parse {name}")))
            .transpose()
    }

    /// The files `conversations.json` is split across, as the manifest
    /// lists them; the one file itself in an export with no manifest.
    pub fn conversation_parts(&mut self) -> Result<Vec<String>> {
        let listed = self.read_json(MANIFEST)?.and_then(|m| {
            m.pointer("/logical_files/conversations.json/files")?
                .as_array()?
                .iter()
                .map(|f| f.as_str().map(String::from))
                .collect::<Option<Vec<String>>>()
        });
        Ok(listed.unwrap_or_else(|| vec![CONVERSATIONS.to_string()]))
    }

    /// Every file under the root named for the file id it holds, by that
    /// id, as a name `read` takes.
    pub fn files_by_id(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for full in &self.names {
            let Some(rel) = full.strip_prefix(&self.root) else {
                continue;
            };
            let base = rel.rsplit('/').next().unwrap_or(rel);
            if let Some(id) = file_id_of_name(base) {
                out.entry(id.to_string()).or_insert_with(|| rel.to_string());
            }
        }
        out
    }
}

/// The folder holding the shallowest manifest, or else the shallowest
/// `conversations.json`: `sites/export_manifest.json` is a part's own.
fn root_of(names: &[String]) -> Option<String> {
    let shallowest = |file: &str| {
        names
            .iter()
            .filter(|n| n.rsplit('/').next() == Some(file))
            .min_by_key(|n| n.matches('/').count())
    };
    let found = shallowest(MANIFEST).or_else(|| shallowest(CONVERSATIONS))?;
    Some(found[..found.len() - found.rsplit('/').next()?.len()].to_string())
}

fn names_under(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                todo.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                let parts: Option<Vec<&str>> = rel
                    .components()
                    .map(|c| match c {
                        Component::Normal(s) => s.to_str(),
                        _ => None,
                    })
                    .collect();
                if let Some(parts) = parts {
                    out.push(parts.join("/"));
                }
            }
        }
    }
    Ok(out)
}

/// The export names a file for its id, then the extension, or a `-` and
/// an upload name or a uuid first: `file_00ab12.dat`, `file-AbC123-notes.pdf`.
pub fn file_id_of_name(name: &str) -> Option<&str> {
    let rest = name
        .strip_prefix("file-")
        .or_else(|| name.strip_prefix("file_"))?;
    let end = rest.find(['-', '.']).unwrap_or(rest.len());
    (end > 0).then(|| &name[..name.len() - rest.len() + end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(path: &Path, files: &[(&str, &str)]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, body) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn a_file_id_is_read_off_the_name_the_export_gives_it() {
        assert_eq!(file_id_of_name("file_00ab12.dat"), Some("file_00ab12"));
        assert_eq!(
            file_id_of_name("file-AbC123-notes.pdf"),
            Some("file-AbC123")
        );
        assert_eq!(
            file_id_of_name("file_00ab12-2f6e7a9c-1111-4222-8333-944445555666.png"),
            Some("file_00ab12")
        );
        assert_eq!(file_id_of_name("file-"), None);
        assert_eq!(file_id_of_name("conversations.json"), None);
    }

    /// The export as ChatGPT sends it: `conversations.json` split into
    /// parts the manifest lists, files as `file_<id>.dat`, and a part's
    /// own manifest one folder down that is not the root's.
    #[test]
    fn a_zip_is_read_in_place_with_its_parts_as_the_manifest_lists_them() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("export.zip");
        let manifest = r#"{"logical_files": {"conversations.json":
            {"files": ["conversations-000.json", "conversations-001.json"], "sharded": true}}}"#;
        zip_of(
            &z,
            &[
                ("export_manifest.json", manifest),
                ("sites/export_manifest.json", "{}"),
                ("conversations-000.json", "[]"),
                ("conversations-001.json", "[]"),
                ("file_00ab12.dat", "bytes"),
            ],
        );
        let mut a = ExportArchive::open(&z).unwrap();
        assert_eq!(
            a.conversation_parts().unwrap(),
            ["conversations-000.json", "conversations-001.json"]
        );
        assert_eq!(a.files_by_id()["file_00ab12"], "file_00ab12.dat");
        assert_eq!(
            a.read("file_00ab12.dat").unwrap().as_deref(),
            Some(&b"bytes"[..])
        );
        assert_eq!(a.read("user.json").unwrap(), None);
    }

    /// A zip of the unzipped folder has every name one folder down; it
    /// reads the same as the folder.
    #[test]
    fn a_zip_of_the_unzipped_folder_reads_the_same() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("export.zip");
        zip_of(
            &z,
            &[
                ("chatgpt-export/conversations.json", "[]"),
                ("chatgpt-export/user-x/file-AbC1-notes.txt", "hello"),
            ],
        );
        let mut a = ExportArchive::open(&z).unwrap();
        assert_eq!(a.conversation_parts().unwrap(), ["conversations.json"]);
        assert_eq!(a.files_by_id()["file-AbC1"], "user-x/file-AbC1-notes.txt");
        assert_eq!(
            a.read("user-x/file-AbC1-notes.txt").unwrap().as_deref(),
            Some(&b"hello"[..])
        );
    }

    /// Pointing the source at the wrong file says what it wanted.
    #[test]
    fn a_zip_with_no_export_in_it_names_what_it_looked_for() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("other.zip");
        zip_of(&z, &[("notes.txt", "hi")]);
        let err = format!("{:#}", ExportArchive::open(&z).err().unwrap());
        assert!(err.contains("export_manifest.json"), "{err}");
        assert!(err.contains("export.path"), "{err}");
        let not_a_zip = d.path().join("notes.txt");
        std::fs::write(&not_a_zip, "hi").unwrap();
        let err = format!("{:#}", ExportArchive::open(&not_a_zip).err().unwrap());
        assert!(err.contains("not a folder or a readable zip"), "{err}");
    }
}
