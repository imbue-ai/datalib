//! The copy a command keeps of a config file before it rewrites it. One
//! per rewrite, named for when it was made, so a run of rewrites keeps
//! every state they passed through.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Writes `text` beside `path` as `<file>.<UTC time>.bak`, owner-only, and
/// returns where. Never replaces a backup already there.
pub fn keep(path: &Path, text: &str) -> std::io::Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    for n in 1.. {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{stamp}"));
        if n > 1 {
            name.push(format!("-{n}"));
        }
        name.push(".bak");
        let bak = PathBuf::from(name);
        match datalib_dag::config::owner_only_options()
            .create_new(true)
            .open(&bak)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(bak);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("an unbounded range")
}
