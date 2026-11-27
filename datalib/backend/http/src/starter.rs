//! The starter `config.toml` the app gives a new library: Datalib's
//! grid and search index, with nothing to read yet, and the applets every
//! app needs. `datalib-http --init` and the first-run screen write it.
//! `datalib-pipeline init` writes a config with no steps instead.

use std::io::Write;
use std::path::Path;

use datalib_config::{owner_only_options, root_config_path};

/// Non-empty on purpose: the index step is source-independent and belongs
/// in every pipeline. It reads every source by pattern, so adding a source
/// only writes it above this block. `data_root` is left out: it defaults
/// to the file's own directory, keeping the root self-contained.
pub const STARTER_CONFIG: &str = r##"# ── the unified grid and search index ──────────────────────────────────
# A group is one thing on the Manage screen; its steps are what run.
# Every source feeds this one — the grid reads its rendered markdown
# and puts it in the keyword search. A step's `id` is the tree it
# writes, and its first part is its group; `inputs` names the steps it
# reads by that id, or every step of one function at once:
# `*/render_markdown` is each source's render, those added later too.
# `command` is the program a step runs, and `datalib-step` is the one
# Datalib ships: `datalib-step run <function>`.
#
# Sources go above this block: a [[groups]] entry with a `type`, then
# its steps. The file then reads in the order data flows, each step
# below the steps it reads, which is where the Sources screen writes
# them and the order it lists them in. The runner follows `inputs`,
# not the file. Anything above the first [[…]] header is a top-level
# key (data_root, binary_dir), not part of an entry. See
# `datalib-step help config-model`.

[[programs]]
command = "datalib-step"
verbs = ["reset", "prepare", "compact", "collect", "check"]

[[locks]]
name = "index"
slots = 2

[[groups]]
id = "unified_grid_and_search_index"
name = "Grid and Search Index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "datalib-step run unified_grid_and_search_index"
inputs = ["*/render_markdown"]
reads = "pinned"
locks = ["index"]

# The applet that serves the grid: the app has no search, no document
# view and no document picker without it. It is an applet like any
# other — a server the gateway spawns and proxies at
# `/applet/unified_grid_and_search_index/` — which is why it is written
# here rather than compiled into the backend.

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"

# The contacts app: the contacts a person makes and the handles they
# link to them, which the person chips and search read. Without it the
# chips still show what each source says about a person, with nothing
# to link. Its store is `datalib_curated/datalib_contacts/`.

[[applets]]
group = "unified_grid_and_search_index"
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
