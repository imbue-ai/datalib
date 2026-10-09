//! The latchkey plugins datalib ships, and putting one where latchkey
//! looks. latchkey loads a plugin only from `<latchkey dir>/plugins/<name>/`,
//! so a service it has no built-in support for — Garmin — exists for it
//! only once the plugin's files are there. The wizard writes them the
//! first time someone signs in to that service, with a stamp saying
//! datalib put them there; a copy without the stamp is the person's own
//! and is never touched.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use datalib_runtime::atomic;

/// One plugin, embedded in this binary. `third-party/<name>/README.md`
/// says where its files come from.
pub struct Plugin {
    /// The latchkey service it adds, which is also its directory name.
    pub service: &'static str,
    /// The upstream commit the files were copied from. The stamp records
    /// it, and a stamp naming another commit is replaced on the next
    /// sign-in.
    pub version: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    /// What `latchkey services info` reports for the service once the
    /// plugin is in place, so the wizard can offer the same ways in
    /// before it is.
    pub auth_options: &'static [&'static str],
    pub set_example: &'static str,
}

pub const GARMIN: Plugin = Plugin {
    service: "garmin",
    version: "0928ac064ab8ad33bb365ed492556cc72b736aec",
    files: &[
        (
            "package.json",
            include_str!(concat!(env!("LATCHKEY_GARMIN_DIR"), "/package.json")),
        ),
        (
            "dist/index.js",
            include_str!(concat!(env!("LATCHKEY_GARMIN_DIR"), "/dist/index.js")),
        ),
        (
            "dist/garmin.js",
            include_str!(concat!(env!("LATCHKEY_GARMIN_DIR"), "/dist/garmin.js")),
        ),
        (
            "dist/oauth1.js",
            include_str!(concat!(env!("LATCHKEY_GARMIN_DIR"), "/dist/oauth1.js")),
        ),
    ],
    auth_options: &["browser", "set"],
    set_example: "latchkey auth set-nocurl garmin ~/.garth",
};

const PLUGINS: &[&Plugin] = &[&GARMIN];

pub fn for_service(service: &str) -> Option<&'static Plugin> {
    PLUGINS.iter().copied().find(|p| p.service == service)
}

/// The file that marks a plugin directory as datalib's. It holds the
/// version installed, or [`PARTIAL`] while the files are being written,
/// so an install cut short is finished next time rather than mistaken
/// for the person's own copy.
const STAMP: &str = ".datalib-installed";
const PARTIAL: &str = "partial";

/// What is at a plugin's directory now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Nothing,
    /// Our stamp, naming the version it holds.
    Ours(String),
    /// Anything without our stamp: a clone, a symlink, a copy someone
    /// made by hand.
    Theirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Write,
    Current,
    LeaveAlone,
}

pub fn plan(found: &Found, version: &str) -> Plan {
    match found {
        Found::Nothing => Plan::Write,
        Found::Ours(v) if v == version => Plan::Current,
        Found::Ours(_) => Plan::Write,
        Found::Theirs => Plan::LeaveAlone,
    }
}

/// latchkey's own directory: `$LATCHKEY_DIRECTORY`, else `~/.latchkey`.
pub fn latchkey_dir() -> Option<PathBuf> {
    latchkey_dir_from(
        std::env::var_os("LATCHKEY_DIRECTORY"),
        std::env::var_os("HOME"),
    )
}

fn latchkey_dir_from(dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    match dir.filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(home.filter(|h| !h.is_empty())?).join(".latchkey")),
    }
}

pub fn plugin_dir(latchkey_dir: &Path, plugin: &Plugin) -> PathBuf {
    latchkey_dir.join("plugins").join(plugin.service)
}

fn found(dir: &Path) -> io::Result<Found> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Found::Nothing),
        Err(e) => return Err(e),
        Ok(meta) if !meta.is_dir() => return Ok(Found::Theirs),
        Ok(_) => {}
    }
    match std::fs::read_to_string(dir.join(STAMP)) {
        Ok(version) => Ok(Found::Ours(version.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Found::Theirs),
        Err(e) => Err(e),
    }
}

/// Puts `plugin` in `dir` unless what is there is current or not ours.
/// Returns what it found there.
pub fn install(plugin: &Plugin, dir: &Path) -> io::Result<Found> {
    let there = found(dir)?;
    if plan(&there, plugin.version) != Plan::Write {
        return Ok(there);
    }
    std::fs::create_dir_all(dir)?;
    atomic::write(&dir.join(STAMP), PARTIAL.as_bytes())?;
    for (rel, text) in plugin.files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic::write(&path, text.as_bytes())?;
    }
    atomic::write(&dir.join(STAMP), plugin.version.as_bytes())?;
    Ok(there)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_is_written_only_over_nothing_or_an_older_copy_of_ours() {
        let v = "abc";
        assert_eq!(plan(&Found::Nothing, v), Plan::Write);
        assert_eq!(plan(&Found::Ours("abc".into()), v), Plan::Current);
        assert_eq!(plan(&Found::Ours("old".into()), v), Plan::Write);
        assert_eq!(plan(&Found::Ours(PARTIAL.into()), v), Plan::Write);
        assert_eq!(plan(&Found::Theirs, v), Plan::LeaveAlone);
    }

    #[test]
    fn latchkeys_directory_is_its_variable_else_under_home() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            latchkey_dir_from(some("/lk"), some("/home/picard")),
            Some(PathBuf::from("/lk"))
        );
        assert_eq!(
            latchkey_dir_from(some(""), some("/home/picard")),
            Some(PathBuf::from("/home/picard/.latchkey"))
        );
        assert_eq!(latchkey_dir_from(None, None), None);
    }

    #[test]
    fn an_install_writes_every_file_and_a_second_is_a_no_op() {
        let td = tempfile::tempdir().unwrap();
        let dir = plugin_dir(td.path(), &GARMIN);
        assert_eq!(install(&GARMIN, &dir).unwrap(), Found::Nothing);
        for (rel, text) in GARMIN.files {
            assert_eq!(std::fs::read_to_string(dir.join(rel)).unwrap(), *text);
        }
        assert_eq!(
            install(&GARMIN, &dir).unwrap(),
            Found::Ours(GARMIN.version.into())
        );
    }

    /// Guards the person's own plugin: a clone they keep up to date
    /// themselves must survive a sign-in from the wizard.
    #[test]
    fn a_copy_without_our_stamp_is_left_exactly_as_it_was() {
        let td = tempfile::tempdir().unwrap();
        let dir = plugin_dir(td.path(), &GARMIN);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("dist/index.js"), "// mine").unwrap();
        assert_eq!(install(&GARMIN, &dir).unwrap(), Found::Theirs);
        assert_eq!(
            std::fs::read_to_string(dir.join("dist/index.js")).unwrap(),
            "// mine"
        );
        assert!(!dir.join(STAMP).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_plugin_is_theirs() {
        let td = tempfile::tempdir().unwrap();
        let clone = td.path().join("clone");
        std::fs::create_dir(&clone).unwrap();
        let dir = plugin_dir(&td.path().join("lk"), &GARMIN);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&clone, &dir).unwrap();
        assert_eq!(install(&GARMIN, &dir).unwrap(), Found::Theirs);
        assert_eq!(std::fs::read_dir(&clone).unwrap().count(), 0);
    }

    #[test]
    fn an_older_copy_of_ours_is_replaced() {
        let td = tempfile::tempdir().unwrap();
        let dir = plugin_dir(td.path(), &GARMIN);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join(STAMP), "old").unwrap();
        std::fs::write(dir.join("dist/index.js"), "// old").unwrap();
        install(&GARMIN, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(STAMP)).unwrap(),
            GARMIN.version
        );
        assert_ne!(
            std::fs::read_to_string(dir.join("dist/index.js")).unwrap(),
            "// old"
        );
    }

    /// The wizard offers Garmin's ways in from these before latchkey can
    /// be asked; they must say what the vendored plugin says.
    #[test]
    fn garmins_advertised_service_matches_the_vendored_plugin() {
        let garmin_js = GARMIN
            .files
            .iter()
            .find(|(rel, _)| *rel == "dist/garmin.js")
            .unwrap()
            .1;
        assert!(garmin_js.contains("name = 'garmin';"));
        assert!(garmin_js.contains("getSession(appNamePrefix)"));
        assert!(garmin_js.contains("getCredentialsNoCurl(noCurlArguments)"));
        // latchkey's `detectsLoginAccount`, which `connect::plugin_service_info`
        // reports as `AccountNaming::Service` before the plugin is in.
        assert!(garmin_js.contains("async getAccount(apiCredentials)"));
        let example = GARMIN.set_example.replace("garmin", "${serviceName}");
        assert!(garmin_js.contains(&format!("return `{example}`;")));
    }
}
