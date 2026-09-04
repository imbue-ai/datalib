//! The plain data the tool moves around.
//!
//! Two shapes matter. [`Inputs`] is everything that was *read* from a
//! doltlite store, before anything is interpreted. [`DiffResult`] is
//! everything that was *concluded*. [`crate::analyze::analyze`] is the
//! pure function between them, and both the HTML page and `--json` are
//! projections of the result rather than things the analysis knows
//! about.
//!
//! Every type here derives `Serialize` + `Deserialize`, so the JSON is
//! the representation rather than a debug dump: a run captured with
//! `--json` deserializes straight back into a `DiffResult`.

use serde::{Deserialize, Serialize};

/// What a node is, on one side of the comparison.
///
/// Serializes to the snake_case strings the viewer switches on; keep
/// them in step with `BADGE` in `viewer.html.tmpl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Same bytes, gone from here and present at [`Node::peer`].
    MovedOut,
    /// Same bytes, arrived here from [`Node::peer`].
    MovedIn,
    /// Gone, and these bytes are nowhere on the other side.
    Removed,
    /// Gone from here, but identical bytes still live at [`Node::peer`].
    RemovedButCopyRemains,
    /// New content — these bytes are nowhere on the other side.
    Added,
    /// New at this path, but the bytes already existed at [`Node::peer`].
    AddedFromCopy,
    /// Same path, different content.
    Modified,
    /// A container that exists only to hold something below it. A
    /// directory row whose digest moved is always this: its digest
    /// covers its children, so "modified" only ever means "something
    /// below me changed", which the children already say.
    Structure,
    /// Present and untouched. Only rendered under `--full-tree`, or
    /// when it carries a duplicate annotation.
    Unchanged,
}

impl Status {
    /// Whether this status is a finding rather than scaffolding.
    pub fn is_finding(self) -> bool {
        !matches!(self, Status::Structure | Status::Unchanged)
    }
}

/// One row of `files`, on one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: String,
    pub size: i64,
    /// Hex digest as doltlite's `hex()` renders it. Only ever compared
    /// for equality.
    pub digest: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// Why a node is also interesting on its own side of the comparison:
/// the same bytes appear elsewhere in the *same* tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupInfo {
    /// How many copies exist, this one included.
    pub n: usize,
    /// The other paths holding these bytes.
    pub peers: Vec<String>,
    /// Bytes reclaimable if every copy but one went away.
    pub waste: i64,
    pub kind: String,
    pub size: i64,
    /// Entries below this one that the rollup absorbed.
    #[serde(default)]
    pub roll: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A rendered tree node. The short field names are what the viewer
/// reads; a page holds one of these per path per side, so the names
/// are terse on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "k")]
    pub kind: String,
    #[serde(rename = "s")]
    pub size: i64,
    #[serde(rename = "st")]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, rename = "roll", skip_serializing_if = "is_zero")]
    pub rolled_up: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dup: Option<DupInfo>,
}

impl Node {
    pub fn plain(path: &str, kind: &str, size: i64, status: Status) -> Self {
        Node {
            path: path.to_string(),
            kind: kind.to_string(),
            size,
            status,
            peer: None,
            note: String::new(),
            rolled_up: 0,
            dup: None,
        }
    }
}

/// The raw prolly diff, split by `diff_type`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub removed: Vec<Entry>,
    pub added: Vec<Entry>,
    pub modified: Vec<(Entry, Entry)>,
}

/// Two or more paths in ONE tree holding byte-identical content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupGroup {
    pub digest: String,
    pub kind: String,
    pub size: i64,
    pub paths: Vec<String>,
    #[serde(default)]
    pub rolled_up: u32,
}

impl DupGroup {
    /// Bytes that would come back if every copy but one went away.
    pub fn wasted(&self) -> i64 {
        (self.paths.len() as i64 - 1) * self.size
    }
}

/// What was read from one side's database, before interpretation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SideInput {
    pub db: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
    /// Every row at this commit. `None` means "changed paths and their
    /// ancestors only", which is derived from the diff alone.
    #[serde(default)]
    pub full: Option<Vec<Entry>>,
    /// Rows at or above the duplicate threshold. Empty when the
    /// in-tree duplicate scan is off.
    #[serde(default)]
    pub dup_candidates: Vec<Entry>,
}

/// Every byte read from both databases, plus the flags that shaped it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inputs {
    pub left: SideInput,
    pub right: SideInput,
    pub diff: Diff,
    /// digest -> a path on the right still holding those bytes.
    #[serde(default)]
    pub copies_right: std::collections::BTreeMap<String, String>,
    /// digest -> a path on the left still holding those bytes.
    #[serde(default)]
    pub copies_left: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub dup_threshold: i64,
    #[serde(default)]
    pub copy_detection: bool,
    #[serde(default)]
    pub unified: bool,
}

/// One side's conclusions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideResult {
    pub db: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
    /// The groups in full, not just a count — otherwise the JSON is
    /// lossy and could not rebuild what `analyze` produced.
    pub dup_groups: Vec<DupGroup>,
    pub dup_wasted: i64,
    pub nodes: Vec<Node>,
}

/// The headline counts. A struct rather than a map so the viewer and
/// the tests agree on the field names by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// Top-level moves, after the subtree rollup.
    pub moves: usize,
    /// Matched pairs before the rollup.
    pub moved_entries: usize,
    /// Entries absorbed by a rollup, across moves, copies and dups.
    pub rolled_up: u32,
    pub removed: usize,
    pub removed_but_copy_remains: usize,
    pub added: usize,
    pub added_from_copy: usize,
    pub modified: usize,
    pub full_tree: bool,
    pub copy_detection: bool,
    pub dup_threshold: i64,
    pub unified: bool,
}

/// The whole comparison as plain data. No SQL, no HTML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResult {
    pub left: SideResult,
    pub right: SideResult,
    pub summary: Summary,
}

impl DiffResult {
    /// The node at `path` on one side. For tests and probes.
    pub fn node(&self, side: Side, path: &str) -> Option<&Node> {
        self.side(side).nodes.iter().find(|n| n.path == path)
    }

    pub fn side(&self, side: Side) -> &SideResult {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// path -> status for one side, ignoring structural filler and
    /// untouched entries. What most assertions want.
    pub fn statuses(&self, side: Side) -> std::collections::BTreeMap<String, Status> {
        self.side(side)
            .nodes
            .iter()
            .filter(|n| n.status.is_finding())
            .map(|n| (n.path.clone(), n.status))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}
