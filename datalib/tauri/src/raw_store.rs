//! What Browse does with a download step's raw store in the app: opens it
//! read-only in DB Browser for SQLite when that is the file's handler, and
//! otherwise in the bundled doltlite shell in a terminal. Read-only either
//! way, because the store may be one a sync is writing, and a second writer
//! breaks the one-writer-per-file rule (AGENTS.md §"Doltlite").
//!
//! Nothing in this file may reference `tauri` or another module of the
//! shell: `//datalib/tauri:raw_store_test` compiles it alone.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// DB Browser for SQLite's bundle id. The stock build shares it but
/// does not claim `.doltlite_db`, so as that file's handler it is
/// DoltHub's doltlite build, which reads our stores and takes `-R`.
pub const DB_BROWSER_BUNDLE_ID: &str = "net.sourceforge.sqlitebrowser";

const STORE_EXTENSION: &str = "doltlite_db";

/// The app the OS would open a file with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handler {
    pub app: PathBuf,
    pub bundle_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    DbBrowser { app: PathBuf },
    Shell,
}

/// Any other handler is passed over: nothing says it can open the file
/// read-only, and a writable open of a live store is the one thing
/// this must not do.
pub fn choose(handler: Option<Handler>) -> Launch {
    match handler {
        Some(h) if h.bundle_id.as_deref() == Some(DB_BROWSER_BUNDLE_ID) => {
            Launch::DbBrowser { app: h.app }
        }
        _ => Launch::Shell,
    }
}

/// The store the webview named, if it is a doltlite store inside the
/// data root this app opened. The page is served over HTTP, so the
/// path is checked rather than trusted.
pub fn check_store(root: &Path, requested: &Path) -> Result<PathBuf, String> {
    if requested.extension().and_then(|e| e.to_str()) != Some(STORE_EXTENSION) {
        return Err(format!("{} is not a doltlite store.", requested.display()));
    }
    let store = requested
        .canonicalize()
        .map_err(|e| format!("{}: {e}", requested.display()))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    if !store.starts_with(&root) || !store.is_file() {
        return Err(format!(
            "{} is not a store in this data library.",
            requested.display()
        ));
    }
    Ok(store)
}

/// `open`'s arguments for DB Browser. `-n` because arguments reach only
/// a fresh launch: without it, a DB Browser already running comes to
/// the front and opens nothing.
pub fn db_browser_args(app: &Path, store: &Path) -> Vec<OsString> {
    vec![
        "-n".into(),
        "-a".into(),
        app.into(),
        "--args".into(),
        "-R".into(),
        store.into(),
    ]
}

/// A `.command` script — what Terminal runs when handed one — that
/// deletes itself and replaces itself with the shell on the store.
pub fn shell_script(doltlite: &Path, store: &Path) -> String {
    format!(
        "#!/bin/sh\nrm -f -- \"$0\"\necho {banner}\nexec {doltlite} -readonly {store}\n",
        banner = quote(&format!("{} (read-only)", store.display())),
        doltlite = quote(&doltlite.to_string_lossy()),
        store = quote(&store.to_string_lossy()),
    )
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(bundle_id: Option<&str>) -> Handler {
        Handler {
            app: PathBuf::from("/Applications/Some.app"),
            bundle_id: bundle_id.map(str::to_string),
        }
    }

    #[test]
    fn only_db_browser_is_trusted_with_the_store() {
        assert_eq!(
            choose(Some(handler(Some(DB_BROWSER_BUNDLE_ID)))),
            Launch::DbBrowser {
                app: PathBuf::from("/Applications/Some.app")
            }
        );
        assert_eq!(
            choose(Some(handler(Some("com.example.hexedit")))),
            Launch::Shell
        );
        assert_eq!(choose(Some(handler(None))), Launch::Shell);
        assert_eq!(choose(None), Launch::Shell);
    }

    /// The page names the path, so one outside the root, or one that
    /// is not a store, must be refused — including through `..`.
    #[test]
    fn a_path_outside_the_root_or_not_a_store_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ingest = root.path().join("slack/ingest");
        std::fs::create_dir_all(&ingest).unwrap();
        let store = ingest.join("entities.doltlite_db");
        std::fs::write(&store, b"CTLD").unwrap();
        std::fs::write(ingest.join("notes.txt"), b"").unwrap();
        let outside = other.path().join("entities.doltlite_db");
        std::fs::write(&outside, b"CTLD").unwrap();

        assert_eq!(
            check_store(root.path(), &store).unwrap(),
            store.canonicalize().unwrap()
        );
        assert!(check_store(root.path(), &outside).is_err());
        // Both temp dirs share a parent, so this climbs out of the root
        // and into the other one.
        let escape = ingest
            .join("../../..")
            .join(other.path().file_name().unwrap())
            .join("entities.doltlite_db");
        assert!(escape.exists());
        assert!(check_store(root.path(), &escape).is_err());
        assert!(check_store(root.path(), &ingest.join("notes.txt")).is_err());
        assert!(check_store(root.path(), &ingest.join("gone.doltlite_db")).is_err());
        assert!(check_store(root.path(), &ingest).is_err());
    }

    /// Data roots live under paths like `~/Imbue Dropbox/…`: a space or
    /// a quote in either path must reach the shell as one argument.
    #[test]
    fn the_script_quotes_paths_with_spaces_and_quotes() {
        let script = shell_script(
            Path::new("/App Dir/datalib-doltlite"),
            Path::new("/Users/me/Bob's Dropbox/slack/ingest/entities.doltlite_db"),
        );
        assert!(script.contains(
            "exec '/App Dir/datalib-doltlite' -readonly \
             '/Users/me/Bob'\\''s Dropbox/slack/ingest/entities.doltlite_db'\n"
        ));
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "set -- {}; printf '%s\\n' \"$@\"",
                quote("/Users/me/Bob's Dropbox/x.doltlite_db")
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "/Users/me/Bob's Dropbox/x.doltlite_db\n"
        );
    }

    #[test]
    fn db_browser_opens_fresh_and_read_only() {
        let args = db_browser_args(
            Path::new("/Applications/DB.app"),
            Path::new("/r/e.doltlite_db"),
        );
        assert_eq!(
            args,
            [
                "-n",
                "-a",
                "/Applications/DB.app",
                "--args",
                "-R",
                "/r/e.doltlite_db"
            ]
            .map(OsString::from)
            .to_vec()
        );
    }
}
