//! The starter `config.toml` the app gives a new library: datalib's
//! document index with nothing to read yet, and the applets every app
//! needs. `datalib-http --init` and the first-run screen write it.
//! `datalib-dag init` writes a config with no steps instead.

use std::io::Write;
use std::path::Path;

use datalib_dag::config::{owner_only_options, root_config_path};

/// Non-empty on purpose: the two index steps are source-independent and
/// belong in every pipeline. Adding a source adds its steps' ids to their
/// `inputs` (the app's "Add a source" and `datalib-step examples --append`
/// both do) and writes the source above this block. `data_root` is left
/// out: it defaults to the file's own directory, keeping the root
/// self-contained.
pub const STARTER_CONFIG: &str = r##"# ── the unified document index ─────────────────────────────────────────
# A group is one thing on the Manage screen; its steps are what run.
# Every source feeds these two — the grid reads its rendered markdown,
# the qmd aggregator its search steps. A step's `id` is the tree it
# writes, and its first part is its group; `inputs` names the steps it
# reads by that id. `command` is the program a step runs, and
# `datalib-step` is the one datalib ships: `datalib-step run <function>`.
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
verbs = ["reset", "prepare", "compact", "collect"]

[[locks]]
name = "index"
slots = 2

[[locks]]
name = "qmd_keyword"
slots = 1

[[groups]]
id = "unified_document_index"
name = "Document Index"

[[steps]]
id = "unified_document_index/grid_index"
command = "datalib-step run grid_index"
inputs = []
reads = "pinned"
locks = ["index"]

[[steps]]
id = "unified_document_index/qmd_aggregator"
command = "datalib-step run qmd_aggregator"
inputs = []
reads = "pinned"
locks = ["qmd_keyword"]

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
