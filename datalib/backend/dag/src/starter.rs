//! The starter `config.toml` a new data root gets: the index group's two
//! fan-in steps, with nothing to read yet, and the applets every app
//! needs. `datalib-dag init`, `datalib-http --init` and the app's
//! first-run screen all write this one text.

use std::io::Write;
use std::path::Path;

use crate::config::{owner_only_options, root_config_path};

/// Non-empty on purpose: the two index steps are source-independent and
/// belong in every pipeline. Adding a source adds its render step's id to
/// their `inputs` (the app's "Add a source" and `datalib-step examples
/// --append` both do) and writes the source above this block.
/// `data_root` is left out: it defaults to the file's own directory,
/// keeping the root self-contained.
pub const STARTER_CONFIG: &str = r##"# ── the unified document index ─────────────────────────────────────────
# A group is one thing on the Manage screen; its steps are what run.
# Every source feeds these two — the grid reads its rendered markdown,
# the qmd aggregator its search steps. A step's id is
# `<group>/<function>`, the tree it writes, and `inputs` names the
# steps it reads by that id. A step with no `command` is one of
# datalib's own.
#
# Sources go above this block: a [[groups]] entry with a `type`, then
# its steps. The file then reads in the order data flows, each step
# below the steps it reads, which is where the Sources screen writes
# them and the order it lists them in. The runner follows `inputs`,
# not the file. Anything above the first [[…]] header is a top-level
# key (data_root, binary_dir), not part of an entry. See
# <origin>/agent/config.md.

[[groups]]
id = "unified_document_index"
name = "Document Index"

[[steps]]
group = "unified_document_index"
function = "grid_index"
inputs = []

[[steps]]
group = "unified_document_index"
function = "qmd_aggregator"
inputs = []

# The applet that serves the grid: the app has no search, no document
# view and no document picker without it. It is an applet like any
# other — a server the gateway spawns and proxies at
# `/applet/unified_document_index/` — which is why it is written here rather
# than compiled into the backend.

[[applets]]
group = "unified_document_index"
id = "unified_document_index"
command = "datalib-applet unified_document_index"

# The contacts app: the contacts a person makes and the handles they
# link to them, which the person chips and search read. Without it the
# chips still show what each source says about a person, with nothing
# to link. Its store is `datalib_curated/datalib_contacts/`.

[[applets]]
group = "unified_document_index"
id = "datalib_contacts"
command = "datalib-applet datalib_contacts"
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

    /// `datalib-dag init` promises a root holding only its config: no
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
