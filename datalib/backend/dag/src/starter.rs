//! The `config.toml` `datalib-pipeline init` writes into a new data root: no
//! steps, and comments saying what an entry is. A program that sets up
//! roots of its own (datalib's app) writes its own starter instead.

use std::io::Write;
use std::path::Path;

use crate::config::{owner_only_options, root_config_path};

/// `data_root` is left out: it defaults to the file's own directory,
/// keeping the root self-contained.
pub const STARTER_CONFIG: &str = r##"# A datalib-pipeline config: the steps to run, each a program that writes
# one directory under this data root.
#
#   [[steps]]
#   id      = "weather/fetch"            # the directory it writes
#   command = "./bin/fetch-weather.sh"   # the program, split shell-style
#
#   [[steps]]
#   id      = "weather/chart"
#   command = "python3 bin/chart.py"
#   inputs  = ["weather/fetch"]          # runs again when this changes
#
# docs/dev/step_protocol.md is what a step is given and what it reports.
"##;

/// Write [`STARTER_CONFIG`] as `root`'s config, creating the root, unless
/// a config is already there. True when this call wrote it. Nothing else
/// under the root is touched.
///
/// `create_new` makes the existence check and the write one syscall, so
/// this never overwrites a config that arrived between them.
pub fn write_starter_config(root: &Path) -> std::io::Result<bool> {
    datalib_runtime::layout::create_data_root(root)?;
    match owner_only_options()
        .create_new(true)
        .open(root_config_path(root))
    {
        Ok(mut f) => {
            f.write_all(STARTER_CONFIG.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `datalib-pipeline init` promises a root holding only its config: no
    /// `system/`, no lock, nothing a server would leave. The root may not
    /// exist yet (the desktop app's new library), and the config holds
    /// credentials, so it is owner-only.
    #[test]
    fn a_new_root_gets_the_config_and_nothing_else() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("Datalib").join("Default");
        assert!(write_starter_config(&root).unwrap());
        let entries: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["config.toml"]);
        assert_eq!(
            std::fs::read_to_string(root.join("config.toml")).unwrap(),
            STARTER_CONFIG
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("config.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "config.toml is {mode:o}");
        }
    }

    #[test]
    fn an_existing_config_is_left_as_it_is() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("config.toml"), "# mine\n").unwrap();
        assert!(!write_starter_config(td.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(td.path().join("config.toml")).unwrap(),
            "# mine\n"
        );
    }
}
