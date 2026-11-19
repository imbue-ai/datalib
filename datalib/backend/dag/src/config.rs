//! The DAG config file, `config.toml`: `[[groups]]` the entries are filed
//! under, `[[steps]]` the scheduler runs, and `[[applets]]` the http
//! gateway spawns on demand. This module parses and validates all three;
//! only steps reach the scheduler.
//!
//! `configs/dag_example.toml` is a complete commented example, and
//! `docs/dev/step_protocol.md` is the contract a step command implements.
//!
//! A step's identity is `(group, function)`, and its id is composed from
//! the two — `<group>/<function>` — never written. That id is the tree the
//! step writes and the key its state is recorded under, so both halves are
//! permanent; the group's `name` is the half that is safe to change. A step
//! outside any group is a custom executable and writes its `id` verbatim.
//!
//! Top-level `data_root` / `binary_dir` must be written *above* the first
//! `[[…]]` header, since everything after a table header belongs to that
//! table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::diagnostics::{Diagnostic, EntryKind, EntryRef, Severity};
use crate::disk_space::DiskFloor;
use crate::graph::Graph;
use crate::step::{StepRun, StepSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagConfig {
    /// Root for all artifacts. Defaults to the directory the config file
    /// lives in, so a data root containing its own config is self-contained.
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    /// Directory prepended to `PATH` for every step subprocess, so commands
    /// can name binaries bare. See [`resolve_binary_dir`] for the fallback.
    #[serde(default)]
    pub binary_dir: Option<PathBuf>,
    /// The containers steps and applets are filed under. A source is a group
    /// with a `type`; the unified document index is a group without one.
    #[serde(default)]
    pub groups: Vec<GroupEntry>,
    /// A config with no steps yet is valid — it just runs nothing.
    #[serde(default)]
    pub steps: Vec<StepEntry>,
    /// Long-lived servers contributing the app's frontend and its data
    /// endpoints. Never scheduled: the http gateway spawns one on demand when
    /// a request for its prefix arrives. Empty is normal.
    #[serde(default)]
    pub applets: Vec<AppletEntry>,
    /// Packages this root uses: programs from outside datalib that its
    /// steps and applets run (`docs/dev/packages.md`).
    #[serde(default)]
    pub packages: Vec<PackageEntry>,
    /// Named locks steps hold, beside the ones every config has
    /// (`supervisor::locks::defaults`).
    #[serde(default)]
    pub locks: Vec<LockEntry>,
    /// The verbs each program takes (`[[programs]]`). A step whose program
    /// has no entry is sent none.
    #[serde(default)]
    pub programs: Vec<ProgramEntry>,
    /// Variables handed to every step, below each step's own `env`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How much of the run store (`system/runs/runs.sqlite`: every run's step
    /// states, log lines and metrics) to keep. Omitted means the defaults
    /// in [`RunHistory`].
    #[serde(default)]
    pub run_history: Option<RunHistory>,
    /// The lowest level of its own lines every process of this root
    /// keeps — the server, the runner, each step. Omitted means
    /// `datalib_log_filter::DEFAULT_LEVEL`; `RUST_LOG` in a process's
    /// environment wins over both.
    #[serde(default)]
    pub log_level: Option<datalib_runs::LogLevel>,
    /// The free space the data root's disk must keep (`[disk_space]`):
    /// under its pause line no step starts and running ones are stopped,
    /// until its resume line. Omitted means the defaults in
    /// [`DiskFloor`].
    #[serde(default)]
    pub disk_space: Option<DiskFloor>,
}

/// The retention rule for `system/runs/runs.sqlite`, as a person writes it in
/// `config.toml`. Both run limits apply; the app server's own log
/// lines, which belong to no run, have their own two. The disk samples in
/// `system/disk_stats.sqlite` have one more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunHistory {
    /// Keep at most this many runs, newest first.
    #[serde(default = "RunHistory::default_max_runs")]
    pub max_runs: u32,
    /// Drop a run older than this many days, however few runs there are.
    #[serde(default = "RunHistory::default_max_age_days")]
    pub max_age_days: u32,
    /// Drop a server log line older than this many days.
    #[serde(default = "RunHistory::default_process_log_days")]
    pub process_log_days: u32,
    /// Keep at most this many server log lines, newest first.
    #[serde(default = "RunHistory::default_process_log_lines")]
    pub process_log_lines: u32,
    /// Drop a disk-usage or free-space sample older than this many days.
    #[serde(default = "RunHistory::default_disk_usage_days")]
    pub disk_usage_days: u32,
}

impl Default for RunHistory {
    fn default() -> Self {
        let r = datalib_runs::Retention::default();
        Self {
            max_runs: r.max_runs,
            max_age_days: r.max_age_days,
            process_log_days: r.process_log_days,
            process_log_lines: r.process_log_lines,
            disk_usage_days: Self::default_disk_usage_days(),
        }
    }
}

impl RunHistory {
    fn default_max_runs() -> u32 {
        datalib_runs::Retention::default().max_runs
    }
    fn default_max_age_days() -> u32 {
        datalib_runs::Retention::default().max_age_days
    }
    fn default_process_log_days() -> u32 {
        datalib_runs::Retention::default().process_log_days
    }
    fn default_process_log_lines() -> u32 {
        datalib_runs::Retention::default().process_log_lines
    }
    /// As long as a run is kept, so every run the dashboard still offers
    /// keeps its disk chart.
    fn default_disk_usage_days() -> u32 {
        datalib_runs::Retention::default().max_age_days
    }

    pub fn retention(self) -> datalib_runs::Retention {
        datalib_runs::Retention {
            max_runs: self.max_runs,
            max_age_days: self.max_age_days,
            process_log_days: self.process_log_days,
            process_log_lines: self.process_log_lines,
        }
    }
}

/// One `[[locks]]` entry: a named lock steps hold while they run, so that
/// steps sharing something the graph does not show (an account's rate
/// limit, a GPU) keep apart. `slots` is how many may hold it at once.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    pub name: String,
    #[serde(default = "LockEntry::one")]
    pub slots: usize,
}

impl LockEntry {
    fn one() -> usize {
        1
    }
}

/// A step's `locks`: a list holds each `shared`, one slot apiece; a table
/// says how it holds each, `shared` or `exclusive` (every slot).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum StepLocks {
    Shared(Vec<String>),
    Held(BTreeMap<String, crate::supervisor::locks::Hold>),
}

impl StepLocks {
    pub fn held(&self) -> Vec<(String, crate::supervisor::locks::Hold)> {
        use crate::supervisor::locks::Hold;
        match self {
            StepLocks::Shared(names) => {
                let unique: BTreeSet<&String> = names.iter().collect();
                unique
                    .into_iter()
                    .map(|n| (n.clone(), Hold::Shared))
                    .collect()
            }
            StepLocks::Held(held) => held.iter().map(|(n, h)| (n.clone(), *h)).collect(),
        }
    }
}

/// How a step reads its inputs, which says whether a writer of them may
/// run beside it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Reads {
    /// At a pinned commit: a writer may go on writing meanwhile.
    Pinned,
    /// Files off disk: no writer of what it reads runs beside it, in
    /// either order.
    Files,
}

impl Reads {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// When a step may start on inputs that are still being written.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StartWhen {
    /// On each seal of a producer that streams, and again once it ends.
    OnEachSeal,
    /// Once nothing upstream of it is running or about to: one run on
    /// what its producers finished with, however often they sealed.
    AfterInputsSettle,
}

impl StartWhen {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One `[[groups]]` entry: the thing a person thinks of as "Work Slack".
/// Its `id` is one path segment and the directory every step under it
/// writes into; its `name` is free text nothing depends on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupEntry {
    pub id: String,
    /// What every screen shows. Never forwarded to a step and never
    /// fingerprinted, so a rename re-runs nothing.
    #[serde(default)]
    pub name: Option<String>,
    /// What kind of thing the group is, for whoever shows it (`slack`,
    /// `email`, …). Opaque to the runner: never forwarded to a step and
    /// never fingerprinted.
    #[serde(default)]
    pub r#type: Option<String>,
    /// What this source is to the person who owns it — "work Slack, mostly
    /// infra channels". Free text, and like `name`: never forwarded to a
    /// step and never fingerprinted. Nothing reads it yet; imbue-ai/datalib#409
    /// is the plan for making search use it.
    #[serde(default)]
    pub description: Option<String>,
    /// Retired: a diff group's source now rides on its steps' commands.
    /// Read only to say so.
    #[serde(default)]
    source: Option<toml::Value>,
}

/// What a config in a retired shape is told to do about it.
pub const RETIRED_SHAPE_HELP: &str = "the app rewrites the file itself when it next reads it, \
     keeping the old one as `config.toml.bak`; from a terminal, \
     `datalib-migrate-config <data root> --force`";

/// One `[[programs]]` entry: the verbs a program takes. A step's program
/// is the first word of its `command`, compared as written.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramEntry {
    pub command: String,
    #[serde(default)]
    /// Spelled as written: a verb this build does not know costs that
    /// verb, with a warning, not the whole file.
    pub verbs: Vec<String>,
}

/// One `[[packages]]` entry: a package this root uses, pinned to one
/// manifest and unpacked under [`PackageEntry::dir_rel`]. A step or applet
/// runs one of its programs by naming the program's path there in its
/// `command`. `docs/dev/packages.md` is the format and the install.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageEntry {
    /// What steps and applets name it by. One path segment.
    pub id: String,
    /// What a person typed to add it (`github:owner/repo`), looked up
    /// again to offer an update. Never read by the runner.
    #[serde(default)]
    pub source: Option<String>,
    /// The manifest this entry is pinned to.
    pub url: String,
    /// The manifest's sha256, which pins every byte the package unpacks:
    /// the manifest names each archive's own sha256.
    pub sha256: String,
}

impl PackageEntry {
    /// Its unpacked directory, relative to the data root.
    pub fn dir_rel(&self) -> String {
        datalib_runtime::layout::package_dir_rel(&self.sha256)
    }
}

/// Why `command` cannot run: its program is in a package directory no
/// `[[packages]]` entry pins, so nothing unpacks it there.
fn unlisted_package(command: &str, packages: &BTreeMap<&str, String>) -> Option<String> {
    let argv0 = command.split_whitespace().next()?;
    let rest = argv0.strip_prefix(datalib_runtime::layout::PACKAGES_DIR_REL)?;
    let dir = rest.strip_prefix('/')?.split('/').next()?;
    let dir = format!("{}/{dir}", datalib_runtime::layout::PACKAGES_DIR_REL);
    (!packages.values().any(|d| *d == dir)).then(|| {
        format!(
            "its program is in {dir}/, which no `[[packages]]` entry pins, so nothing puts \
             it there"
        )
    })
}

/// One applet instance. Deliberately a subset of [`StepEntry`]: an applet
/// declares no `inputs` because it is not scheduled and owns no artifacts.
/// There is no `title` either — the label the gallery shows is written by
/// the applet itself into its namespace metadata.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletEntry {
    /// Instance name. Doubles as the mount prefix (`/applet/<id>/`) *and* as
    /// an identifier injected into card-source scope, so it is restricted to
    /// what JavaScript accepts as a variable name. Not composed from the
    /// group: it has to be globally unique on its own.
    pub id: String,
    /// The group this applet is filed under on the Manage screen. Only a
    /// filing: an applet writes no tree and is never an `inputs` target, so
    /// nothing else reads it.
    #[serde(default)]
    pub group: Option<String>,
    /// The command to run, split shell-style, resolved the same way a
    /// step's is.
    pub command: String,
    /// Arbitrary applet parameters, forwarded verbatim as the JSON in the
    /// applet's `--params-file`.
    #[serde(default)]
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl AppletEntry {
    pub fn params_json(&self) -> Result<Option<serde_json::Value>> {
        match &self.params {
            Some(v) => Ok(Some(params_to_json(v, &self.id)?)),
            None => Ok(None),
        }
    }
}

/// One `[[steps]]` entry (`docs/dev/step_protocol.md` § The config entry).
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "StepTable")]
pub struct StepEntry {
    /// The tree this step writes and the key its state is recorded under.
    /// Its first part is the step's group.
    pub id: String,
    /// Free text the runner never reads.
    pub name: Option<String>,
    /// The ids of the steps this one reads. A step id *is* the tree that step
    /// writes, so an entry here is both a step reference and an artifact path.
    pub inputs: Vec<String>,
    /// The command to run, split shell-style into an argv. A bare program
    /// name is looked up in `binary_dir`, then the library's `system/bin`,
    /// then `PATH`; a relative path with a `/` in it is relative to the
    /// data root, the child's working directory, which is how a step runs
    /// a package's program.
    pub command: String,
    /// Arbitrary step parameters, handed to the step as the JSON in
    /// `DATALIB_DAG_PARAMS`.
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    pub env: BTreeMap<String, String>,
    /// Version of the step's own behavior, for steps whose output can change
    /// without their command line changing. Bumping it re-runs the step once,
    /// even though none of its inputs moved.
    pub code_version: Option<String>,
    /// The named locks it holds while it runs. `None` holds the default
    /// its shape gives it (`supervisor::locks::default_for`).
    pub locks: Option<StepLocks>,
    /// How it reads its inputs. `None` is [`Reads::Files`].
    pub reads: Option<Reads>,
    /// When it may start on inputs still being written. `None` is
    /// [`StartWhen::OnEachSeal`].
    pub start: Option<StartWhen>,
}

/// A `[[steps]]` table exactly as a person writes it. `group` and
/// `function` are the retired shape, read only to say so.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepTable {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    group: Option<toml::Value>,
    #[serde(default)]
    function: Option<toml::Value>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    params: Option<toml::Value>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    code_version: Option<String>,
    #[serde(default)]
    locks: Option<StepLocks>,
    #[serde(default)]
    reads: Option<Reads>,
    #[serde(default)]
    start: Option<StartWhen>,
}

impl TryFrom<StepTable> for StepEntry {
    type Error = String;

    fn try_from(t: StepTable) -> std::result::Result<Self, String> {
        if t.group.is_some() || t.function.is_some() {
            return Err(format!(
                "`group` and `function` are the shape from before a step wrote only its \
                 `id`, `<group>/<function>`: {RETIRED_SHAPE_HELP}"
            ));
        }
        let Some(id) = t.id else {
            return Err("missing field `id`: the directory the step writes".to_string());
        };
        let Some(command) = t.command else {
            return Err(format!(
                "missing field `command`: every step names the program it runs. A step of \
                 datalib's own written before this was required: {RETIRED_SHAPE_HELP}"
            ));
        };
        Ok(StepEntry {
            id,
            name: t.name,
            inputs: t.inputs,
            command,
            params: t.params,
            env: t.env,
            code_version: t.code_version,
            locks: t.locks,
            reads: t.reads,
            start: t.start,
        })
    }
}

impl StepEntry {
    /// The first part of its id, which names its group when a `[[groups]]`
    /// entry has that id. `None` for an id of one part.
    pub fn group(&self) -> Option<&str> {
        group_of(&self.id)
    }

    /// The first word of its command: what a `[[programs]]` entry names.
    pub fn program(&self) -> Option<String> {
        shlex::split(&self.command).and_then(|argv| argv.into_iter().next())
    }
}

/// The first part of a step id, for an id of more than one part.
pub fn group_of(step_id: &str) -> Option<&str> {
    step_id.split_once('/').map(|(g, _)| g)
}

/// The id a `[[steps]]` table would get, read off the raw TOML so a step
/// that fails to deserialize can still be named in its diagnostic.
fn raw_step_id(value: &toml::Value) -> Option<String> {
    let s = |k: &str| value.get(k).and_then(|v| v.as_str());
    match (s("id"), s("group"), s("function")) {
        (Some(id), _, _) => Some(id.to_string()),
        (None, Some(g), Some(f)) => Some(format!("{g}/{f}")),
        _ => None,
    }
}

pub fn root_config_path(data_root: &Path) -> PathBuf {
    data_root.join(CONFIG_FILE_NAME)
}

/// `OpenOptions` for a file only this user may read. The config holds
/// every source's credentials, so it is never left at the umask's mercy.
pub fn owner_only_options() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

/// Create (or truncate) `path` owner-only and write `bytes` to it. A
/// file that already exists keeps its mode: `mode` applies at creation.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = owner_only_options()
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)
}

/// The canonical config filename.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Parse config text, strictly: the first problem that drops an entry is an
/// error. For callers that must not act on a config with a known problem.
/// A warning passes — it drops nothing, and a strict caller acting on the
/// file gets exactly what the graded loader would have run. Anything
/// reporting to a human wants [`parse_graded`].
pub fn parse(text: &str) -> Result<DagConfig> {
    let (cfg, diagnostics) = parse_graded(text);
    if let Some(d) = diagnostics.iter().find(|d| d.severity.drops_the_entry()) {
        bail!("{}", d.describe());
    }
    Ok(cfg)
}

/// Whether this text is TOML that could be a config at all — the file-level
/// question only, with no opinion on whether the config is *valid*
/// ([`check_text`]'s job).
pub fn is_toml(text: &str) -> bool {
    !parse_graded(text)
        .1
        .iter()
        .any(|d| d.severity == Severity::Fatal)
}

/// Load + resolve a config file, strictly. The strict view of
/// [`load_graded`]: one error instead of every problem.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg = parse(&text).with_context(|| format!("parse {}", path.display()))?;
    let root = data_root_of(path, &cfg);
    Ok((cfg, root))
}

/// Read and check a config file, keeping whatever loads. Only I/O failures
/// are `Err`: a file that cannot be read has no diagnostics to give.
pub fn load_graded(path: &Path) -> Result<(ConfigCheck, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let checked = check_text(&text);
    let root = data_root_of(path, &checked.cfg);
    Ok((checked, root))
}

/// Where a config at `path` keeps its data: its `data_root`, else its own
/// directory. Always absolute: a step runs with the root as its working
/// directory, and would resolve a relative one a second time.
pub fn data_root_of(path: &Path, cfg: &DagConfig) -> PathBuf {
    match &cfg.data_root {
        Some(p) => absolutize(expand_tilde(p)),
        None => {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            abs.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        }
    }
}

/// The runner's and the server's own state. A step writing there would put
/// the scheduler's own bookkeeping under its change detection. This is the
/// policy; the path constants live in `datalib_core::layout`, which this
/// crate deliberately doesn't depend on.
pub const SYSTEM_DIR: &str = "system";

/// State a person curates by hand, one directory per app
/// (`datalib_contacts::CURATED_DIR`). Nothing can rebuild it, so no step
/// may write there.
pub const CURATED_DIR: &str = "datalib_curated";

/// Each top-level directory no group or step may claim, and what it holds.
const RESERVED_DIRS: &[(&str, &str)] = &[
    (SYSTEM_DIR, "the runner's and the server's own state"),
    (
        CURATED_DIR,
        "state a person curates by hand, which nothing can rebuild",
    ),
];

fn reserved_dir(top: &str) -> Option<(&'static str, &'static str)> {
    RESERVED_DIRS.iter().copied().find(|(dir, _)| *dir == top)
}

/// One id segment: what a directory name may contain. Deliberately narrower
/// than the filesystem allows — an id is a path component on every platform
/// we ship to and appears inside `markdowns.md_path`, so the portable
/// filename character set plus `.` is all it needs to be.
fn valid_id_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && !seg.starts_with('-')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// One segment of an id that names a folder: a [`valid_id_segment`] in
/// lower case. A Mac's disk does not tell `Email/` from `email/`, so
/// with one spelling per folder no two ids can name one folder anywhere.
fn valid_folder_segment(seg: &str) -> bool {
    valid_id_segment(seg) && !seg.chars().any(|c| c.is_ascii_uppercase())
}

/// Whether `function` could be a step's function: one folder name, so
/// `<group>/<function>` stays directly under the group's folder.
pub fn usable_function(function: &str) -> bool {
    valid_folder_segment(function)
}

/// Whether `id` is a group id a config may name: one folder segment,
/// never a reserved directory. What makes `<root>/<id>` that group's tree.
pub fn usable_group_id(id: &str) -> bool {
    valid_folder_segment(id) && reserved_dir(id).is_none()
}

const SEGMENT_RULE: &str = "letters, digits, `.`, `_`, `-`, not starting with `-`, and \
                            never `.` or `..`";

const FOLDER_RULE: &str = "lowercase letters, digits, `.`, `_`, `-`, not starting with \
                           `-`, and never `.` or `..`";

pub fn validate_steps(cfg: &DagConfig) -> Result<()> {
    let accepted = accept_entries(cfg_candidates(cfg), None);
    if let Some(d) = accepted
        .diagnostics
        .iter()
        .find(|d| d.severity.drops_the_entry())
    {
        bail!("{}", d.describe());
    }
    Ok(())
}

/// The strict view of [`accept_entries`]. Note what this does *not* do: it
/// resolves nothing between steps, so an `inputs` entry naming no declared
/// step passes here and is caught by [`crate::Graph::build`].
pub fn to_specs(cfg: &DagConfig) -> Result<Vec<StepSpec>> {
    let accepted = accept_entries(cfg_candidates(cfg), None);
    if let Some(d) = accepted
        .diagnostics
        .iter()
        .find(|d| d.severity.drops_the_entry())
    {
        bail!("{}", d.describe());
    }
    Ok(accepted.steps.into_iter().map(|(_, spec)| spec).collect())
}

/// A step's `params` subtree as the JSON the child reads from its
/// `--params-file`.
///
/// Walks the tree rather than serializing through serde, because TOML's
/// date/time types have no JSON counterpart and the `toml` crate smuggles
/// them past a non-TOML serializer as a one-key map.
fn params_to_json(v: &toml::Value, step: &str) -> Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match v {
        toml::Value::String(s) => J::String(s.clone()),
        toml::Value::Integer(i) => J::from(*i),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => J::Number(n),
            // TOML has nan/inf literals and JSON has no way to say them, so
            // refuse rather than hand the child a null it would read as
            // "unset".
            None => bail!(
                "step {step:?}: params has a non-finite float ({f}), which JSON can't represent"
            ),
        },
        toml::Value::Array(a) => J::Array(
            a.iter()
                .map(|x| params_to_json(x, step))
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(t) => J::Object(
            t.iter()
                .map(|(k, x)| Ok((k.clone(), params_to_json(x, step)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// The directory prepended to every step's `PATH`. Precedence: CLI override,
/// config `binary_dir`, then this executable's own directory. Relative paths
/// are absolutized against the *runner's* cwd, since steps run in `data_root`.
pub fn resolve_binary_dir(cfg: &DagConfig, cli_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = cli_override {
        return Some(absolutize(expand_tilde(p)));
    }
    if let Some(p) = &cfg.binary_dir {
        return Some(absolutize(expand_tilde(p)));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p,
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// The one namespace an applet may not claim. Starting an applet deletes and
/// rewrites its `system/frontend/<namespace>/` directory, and `user` holds
/// hand- and agent-authored components that nothing regenerates.
pub const RESERVED_APPLET_ID: &str = "user";

pub fn validate_applets(cfg: &DagConfig) -> Result<()> {
    let package_dirs: BTreeMap<&str, String> = cfg
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.dir_rel()))
        .collect();
    let (_, diags) = accept_applets(
        candidates(&cfg.applets, EntryRef::applet, |a| Some(a.id.clone())),
        &package_dirs,
        None,
    );
    if let Some(d) = diags.first() {
        bail!("{}", d.describe());
    }
    Ok(())
}

/// A conservative ASCII-only subset of what JavaScript accepts: an id is
/// also a URL path segment and a directory-safe token, so the narrow rule is
/// the useful one.
fn is_js_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => return false,
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        Some(_) => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    // Reserved words would parse as syntax, not as a binding.
    !matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "let"
            | "static"
            | "yield"
            | "await"
    )
}

// ── The graded loader. Above this line is the strict door: first problem
// wins and the caller gets an `Err`. Both run the same rules — the strict
// functions are thin wrappers — so there is one spelling of what is valid.

/// The file's own shape, with the entries left opaque.
///
/// This split is what makes a graded load possible: deserializing straight
/// into [`DagConfig`] makes serde's first objection the whole file's error,
/// because serde cannot skip one entry and keep going. `deny_unknown_fields`
/// stays fatal *here*, where an unknown top-level key is a statement about
/// the file. `Spanned` wraps entries and not their fields, because the
/// location a reader wants is the `[[steps]]` header.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    data_root: Option<PathBuf>,
    #[serde(default)]
    binary_dir: Option<PathBuf>,
    #[serde(default)]
    groups: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    steps: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    applets: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    packages: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    locks: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    programs: Vec<ProgramEntry>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    run_history: Option<RunHistory>,
    #[serde(default)]
    log_level: Option<datalib_runs::LogLevel>,
    #[serde(default)]
    disk_space: Option<DiskFloor>,
    // The retired shape: knobs one step program reads, now set in `[env]`.
    // Read only to say so.
    #[serde(default)]
    checkpoint_cadence: Option<toml::Spanned<toml::Value>>,
    #[serde(default)]
    render_pages: Option<toml::Spanned<toml::Value>>,
    #[serde(default)]
    embed_contexts: Option<toml::Spanned<toml::Value>>,
}

/// One entry on its way in: where it sits in the file, and what it
/// deserialized to. Generic so groups, steps and applets share the
/// bookkeeping — but not the rules, which are not the same rules.
struct Candidate<T> {
    entry: T,
    reference: EntryRef,
    /// Byte range of the entry's header. `None` when the caller came in
    /// through the strict, text-less door, which holds no file to point into.
    span: Option<std::ops::Range<usize>>,
}

impl<T> Candidate<T> {
    fn diag(
        &self,
        severity: Severity,
        text: Option<&str>,
        key: Option<&str>,
        message: impl Into<String>,
    ) -> Diagnostic {
        let d = Diagnostic::new(severity, message).at_entry(self.reference.clone());
        match (text, &self.span) {
            (Some(t), Some(sp)) => {
                let at = match key {
                    Some(k) => key_span(t, sp.clone(), k),
                    None => sp.clone(),
                };
                d.at_span(t, at)
            }
            _ => d,
        }
    }
}

fn candidates<T: Clone>(
    entries: &[T],
    make_ref: fn(usize, Option<String>) -> EntryRef,
    id_of: fn(&T) -> Option<String>,
) -> Vec<Candidate<T>> {
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| Candidate {
            reference: make_ref(i, id_of(e)),
            entry: e.clone(),
            span: None,
        })
        .collect()
}

/// Every entry on its way in, plus which groups anything *wrote* under.
struct Candidates {
    /// Not graded one by one: a program entry is a few words, and the
    /// rules for it are `programs_problems`'.
    programs: Vec<ProgramEntry>,
    locks: Vec<Candidate<LockEntry>>,
    packages: Vec<Candidate<PackageEntry>>,
    groups: Vec<Candidate<GroupEntry>>,
    steps: Vec<Candidate<StepEntry>>,
    applets: Vec<Candidate<AppletEntry>>,
    /// Every group a step's id or an applet's `group` names, counted
    /// before anything is dropped: a group whose only step was rejected is
    /// still a group somebody meant to fill, and calling it empty would
    /// send them to the wrong entry.
    named: BTreeSet<String>,
}

/// The strict door's candidates, from an already-deserialized config.
fn cfg_candidates(cfg: &DagConfig) -> Candidates {
    let mut named = BTreeSet::new();
    named.extend(
        cfg.steps
            .iter()
            .filter_map(|s| s.group().map(str::to_string)),
    );
    named.extend(cfg.applets.iter().filter_map(|a| a.group.clone()));
    Candidates {
        programs: cfg.programs.clone(),
        locks: candidates(&cfg.locks, EntryRef::lock, |l| Some(l.name.clone())),
        packages: candidates(&cfg.packages, EntryRef::package, |p| Some(p.id.clone())),
        groups: candidates(&cfg.groups, EntryRef::group, |g| Some(g.id.clone())),
        steps: candidates(&cfg.steps, EntryRef::step, |e| Some(e.id.clone())),
        applets: candidates(&cfg.applets, EntryRef::applet, |a| Some(a.id.clone())),
        named,
    }
}

/// What survived the entry rules, and what each problem cost.
struct Accepted {
    locks: Vec<LockEntry>,
    packages: Vec<PackageEntry>,
    /// Every lock a step may hold: the defaults, resized or joined by
    /// what the config declares.
    lock_specs: Vec<crate::supervisor::locks::LockSpec>,
    groups: Vec<GroupEntry>,
    steps: Vec<(StepEntry, StepSpec)>,
    applets: Vec<AppletEntry>,
    diagnostics: Vec<Diagnostic>,
}

/// Every rule decidable without the graph, applied entry by entry, dropping
/// what fails and saying why. Groups first, because a step's rules depend on
/// which groups exist; steps and applets after, each against that list.
fn accept_entries(c: Candidates, text: Option<&str>) -> Accepted {
    let mut diags = Vec::new();
    let (locks, lock_specs, lock_diags) = accept_locks(c.locks, text);
    diags.extend(lock_diags);
    let (groups, group_diags) = accept_groups(c.groups, &c.named, text);
    let dropped_groups: BTreeSet<String> = group_diags
        .iter()
        .filter(|d| d.severity.drops_the_entry())
        .filter_map(|d| d.id().map(str::to_string))
        .collect();
    diags.extend(group_diags);

    let (packages, package_diags) = accept_packages(c.packages, text);
    diags.extend(package_diags);
    let package_dirs: BTreeMap<&str, String> = packages
        .iter()
        .map(|p| (p.id.as_str(), p.dir_rel()))
        .collect();

    let (verbs_by_program, program_diags) = accept_programs(&c.programs);
    diags.extend(program_diags);
    let by_id: BTreeMap<&str, &GroupEntry> = groups.iter().map(|g| (g.id.as_str(), g)).collect();
    let (steps, step_diags) = accept_steps(
        c.steps,
        &by_id,
        &dropped_groups,
        &lock_specs,
        &package_dirs,
        &verbs_by_program,
        text,
    );
    diags.extend(step_diags);

    let (applets, applet_diags) = accept_applets(c.applets, &package_dirs, text);
    diags.extend(applet_diags);
    for a in &applets {
        if let Some(g) = &a.group {
            if !by_id.contains_key(g.as_str()) {
                // A warning, not a rejection: the group is only where the
                // Manage screen files the applet, and a wrong filing is not a
                // reason to take the grid down.
                let reference = EntryRef::applet(usize::MAX, Some(a.id.clone()));
                let d = Diagnostic::new(
                    Severity::Warning,
                    format!("group {g:?} names no declared group; the applet is shown ungrouped"),
                )
                .at_entry(reference);
                diags.push(d);
            }
        }
    }

    Accepted {
        locks,
        packages,
        lock_specs,
        groups,
        steps,
        applets,
        diagnostics: diags,
    }
}

/// The package rules: an id is one path segment, unique, the earlier entry
/// keeping a contested one; the pin is a whole sha256.
fn accept_packages(
    candidates: Vec<Candidate<PackageEntry>>,
    text: Option<&str>,
) -> (Vec<PackageEntry>, Vec<Diagnostic>) {
    let mut accepted = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for c in candidates {
        let id = c.entry.id.clone();
        if !valid_id_segment(&id) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!("id {id:?} is not usable: {SEGMENT_RULE}"),
            ));
            continue;
        }
        let sha = &c.entry.sha256;
        if sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("sha256"),
                format!("sha256 {sha:?} is not a sha256: 64 lowercase hex digits"),
            ));
            continue;
        }
        if !seen.insert(id.clone()) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("id"),
                    format!("duplicate id {id:?}"),
                )
                .with_help("the earlier entry keeps this id; give this one another"),
            );
            continue;
        }
        accepted.push(c.entry);
    }
    (accepted, diags)
}

/// The group rules. An id is one directory name, so it is one segment and
/// not `system`; ids are unique, the earlier entry keeping a contested one. A
/// group nothing names is kept, with a warning: it is probably a source
/// somebody deleted the steps of and forgot.
fn accept_groups(
    candidates: Vec<Candidate<GroupEntry>>,
    named: &BTreeSet<String>,
    text: Option<&str>,
) -> (Vec<GroupEntry>, Vec<Diagnostic>) {
    let mut accepted = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for c in candidates {
        let id = c.entry.id.clone();
        if c.entry.source.is_some() {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("source"),
                    "`source` is the shape from before a diff group's steps named their \
                     source's type in their `command`",
                )
                .with_help(RETIRED_SHAPE_HELP),
            );
            continue;
        }
        if !valid_folder_segment(&id) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "group id {id:?} is not a usable directory name. Every step under a \
                     group writes into `<group id>/`, so the id must be one portable \
                     filename — {FOLDER_RULE} — with no `/` in it."
                ),
            ));
            continue;
        }
        if let Some((dir, holds)) = reserved_dir(&id) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "group id {dir:?} is reserved for {holds}; every step under it would \
                     write there."
                ),
            ));
            continue;
        }
        if !seen.insert(id.clone()) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("id"),
                    format!(
                        "duplicate group id {id:?}. Steps name their group by this id, so \
                         two groups sharing one would both claim every step under it."
                    ),
                )
                .with_help("the earlier entry keeps this id; give this one a distinct one"),
            );
            continue;
        }
        if !named.contains(&id) {
            diags.push(c.diag(
                Severity::Warning,
                text,
                None,
                format!("group {id:?} has no steps and no applets under it"),
            ));
        }
        accepted.push(c.entry);
    }
    (accepted, diags)
}

/// The lock rules: a name is one id segment and declared once, and a lock
/// has at least one slot. A declared lock named like a default resizes it.
fn accept_locks(
    candidates: Vec<Candidate<LockEntry>>,
    text: Option<&str>,
) -> (
    Vec<LockEntry>,
    Vec<crate::supervisor::locks::LockSpec>,
    Vec<Diagnostic>,
) {
    let mut accepted: Vec<LockEntry> = Vec::new();
    let mut diags = Vec::new();
    for c in candidates {
        let name = &c.entry.name;
        let problem = if !valid_id_segment(name) {
            Some((
                "name",
                format!("lock name {name:?} is not usable: {SEGMENT_RULE}"),
            ))
        } else if c.entry.slots == 0 {
            Some((
                "slots",
                "a lock needs at least one slot, or nothing holding it could ever run".into(),
            ))
        } else if accepted.iter().any(|l| &l.name == name) {
            Some((
                "name",
                format!("lock {name:?} is declared twice; the first one stands"),
            ))
        } else {
            None
        };
        match problem {
            Some((key, message)) => {
                diags.push(c.diag(Severity::Rejected, text, Some(key), message))
            }
            None => accepted.push(c.entry),
        }
    }
    let mut specs = crate::supervisor::locks::defaults();
    for l in &accepted {
        match specs.iter_mut().find(|s| s.name == l.name) {
            Some(default) => default.slots = l.slots,
            None => specs.push(crate::supervisor::locks::LockSpec {
                name: l.name.clone(),
                slots: l.slots,
            }),
        }
    }
    (accepted, specs, diags)
}

/// The `[[programs]]` rules: a program is named once, and a verb once per
/// program. Returns each program's verbs.
fn accept_programs(
    programs: &[ProgramEntry],
) -> (
    BTreeMap<String, BTreeSet<crate::step::Verb>>,
    Vec<Diagnostic>,
) {
    let mut out: BTreeMap<String, BTreeSet<crate::step::Verb>> = BTreeMap::new();
    let mut diags = Vec::new();
    for p in programs {
        let command = p.command.trim();
        if command.is_empty() || command.split_whitespace().count() != 1 {
            diags.push(Diagnostic::new(
                Severity::Rejected,
                format!(
                    "[[programs]] command {command:?} is not one program: it names the first \
                     word of the steps' `command` it applies to"
                ),
            ));
            continue;
        }
        if out.contains_key(command) {
            diags.push(Diagnostic::new(
                Severity::Rejected,
                format!("[[programs]] {command:?} is declared twice; the first one stands"),
            ));
            continue;
        }
        let mut verbs = BTreeSet::new();
        for word in &p.verbs {
            match crate::step::Verb::parse(word) {
                Some(verb) => {
                    verbs.insert(verb);
                }
                None => diags.push(Diagnostic::new(
                    Severity::Warning,
                    format!(
                        "[[programs]] {command:?}: {word:?} is not a verb this runner sends \
                         (reset, prepare, compact, collect); it is ignored"
                    ),
                )),
            }
        }
        out.insert(command.to_string(), verbs);
    }
    (out, diags)
}

/// The step rules, applied entry by entry. A step's `id` must be a usable
/// path; it is unique, un-nested with every other, and outside `system/`;
/// the command must be runnable and `params` must be JSON-able and small.
///
/// The nesting rule is the load-bearing one: two steps under one tree is two
/// writers on one `.doltlite_db`, sharing the working set of the branch they
/// both open on, so they commit each other's in-flight rows.
///
/// Later entries lose to earlier ones. Inputs are checked in
/// [`crate::Graph::build_graded`], the first place the full id set exists.
fn accept_steps(
    candidates: Vec<Candidate<StepEntry>>,
    groups: &BTreeMap<&str, &GroupEntry>,
    dropped_groups: &BTreeSet<String>,
    locks: &[crate::supervisor::locks::LockSpec],
    packages: &BTreeMap<&str, String>,
    programs: &BTreeMap<String, BTreeSet<crate::step::Verb>>,
    text: Option<&str>,
) -> (Vec<(StepEntry, StepSpec)>, Vec<Diagnostic>) {
    let mut accepted: Vec<(StepEntry, StepSpec)> = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for c in candidates {
        let id = c.entry.id.clone();
        if id.is_empty() || !id.split('/').all(valid_folder_segment) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "id {id:?} is not a usable directory name. An id is the directory the \
                     step writes, so every `/`-separated segment must be a portable filename \
                     — {FOLDER_RULE}."
                ),
            ));
            continue;
        }
        let group = c
            .entry
            .group()
            .filter(|g| groups.contains_key(g))
            .map(str::to_string);
        if let Some(g) = c
            .entry
            .group()
            .filter(|g| dropped_groups.contains(*g) && !groups.contains_key(g))
        {
            diags.push(
                c.diag(
                    Severity::Blocked,
                    text,
                    Some("id"),
                    format!("cannot run: its group {g:?} was itself dropped from this config"),
                )
                .with_help(format!(
                    "nothing is wrong with this step — fix group {g:?} and this one runs again"
                )),
            );
            continue;
        }
        let top = id.split('/').next().unwrap_or_default();
        if let Some((dir, holds)) = reserved_dir(top) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!("id {id:?} writes under {dir:?}, which is reserved for {holds}."),
            ));
            continue;
        }
        if seen.contains(&id) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("id"),
                    format!(
                        "duplicate id {id:?}. A step's id is both its bookkeeping key and the \
                         tree it writes, so two steps sharing one would overwrite each \
                         other's state and each other's output."
                    ),
                )
                .with_help("the earlier entry keeps this id; give this one a distinct one"),
            );
            continue;
        }
        // Containment, which the string equality above cannot see. Checked
        // both ways: either direction is two steps writing one tree.
        if let Some(other) = seen.iter().find(|other| nests_with(other, &id)) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("id"),
                    format!(
                        "id {id:?} is nested with step {other:?}: one of these trees contains \
                         the other, so both steps write the same files. A step's id *is* the \
                         tree it writes, and every tree has exactly one writer."
                    ),
                )
                .with_help(
                    "move one of them out from under the other — sibling steps under one \
                     group, like `slack/ingest` and `slack/render_markdown`, are the usual \
                     shape",
                ),
            );
            continue;
        }
        if let Some(why) = unlisted_package(&c.entry.command, packages) {
            diags.push(c.diag(Severity::Rejected, text, Some("command"), why));
            continue;
        }
        let spec = match spec_of(&c.entry, group, programs) {
            Ok(spec) => spec,
            Err(e) => {
                diags.push(c.diag(Severity::Rejected, text, Some("command"), format!("{e:#}")));
                continue;
            }
        };
        let undeclared: Vec<String> = crate::supervisor::locks::held_by(&spec)
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !locks.iter().any(|l| &l.name == name))
            .collect();
        if !undeclared.is_empty() {
            let declared: Vec<&str> = locks.iter().map(|l| l.name.as_str()).collect();
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("locks"),
                    format!(
                        "holds lock(s) no `[[locks]]` entry declares: {}",
                        undeclared.join(", ")
                    ),
                )
                .with_help(format!(
                    "declare each with `[[locks]]` and a `name`; declared: {}",
                    declared.join(", ")
                )),
            );
            continue;
        }
        seen.insert(id);
        accepted.push((c.entry, spec));
    }
    (accepted, diags)
}

/// The byte range of a step's `[[steps]]` header in the config text, for
/// a problem found after the check. `None` for a step the text does not
/// declare.
pub fn step_header_span(text: &str, step_id: &str) -> Option<std::ops::Range<usize>> {
    entries_of(text).spans.get(step_id).cloned()
}

fn nests_with(a: &str, b: &str) -> bool {
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// The most `params` a step may have, as JSON: they travel in an
/// environment variable, and Linux refuses one string over 128 KiB.
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;

fn spec_of(
    e: &StepEntry,
    group: Option<String>,
    programs: &BTreeMap<String, BTreeSet<crate::step::Verb>>,
) -> Result<StepSpec> {
    let argv = shlex::split(&e.command)
        .with_context(|| format!("command {:?} has unbalanced quoting", e.command))?;
    let Some(program) = argv.first() else {
        bail!("empty command");
    };
    let params = match &e.params {
        Some(params) => {
            let json =
                serde_json::to_string(&params_to_json(params, &e.id)?).context("params → JSON")?;
            if json.len() > MAX_PARAMS_BYTES {
                bail!(
                    "params are {} bytes as JSON; a step's params may be at most {} KiB",
                    json.len(),
                    MAX_PARAMS_BYTES / 1024
                );
            }
            Some(json)
        }
        None => None,
    };
    let verbs = programs.get(program).cloned().unwrap_or_default();
    let mut spec = StepSpec::new(
        &e.id,
        StepRun::Subprocess {
            argv,
            env: e.env.clone(),
            params,
        },
    );
    spec.code_version = e.code_version.clone();
    spec.verbs = verbs;
    spec.group = group;
    spec.reads_pinned = e.reads == Some(Reads::Pinned);
    spec.waits_for_settled_inputs = e.start == Some(StartWhen::AfterInputsSettle);
    spec.locks = e.locks.as_ref().map(StepLocks::held);
    for i in &e.inputs {
        spec.inputs.push(crate::ArtifactPath::parse(i)?);
    }
    Ok(spec)
}

/// The applet rules, applied entry by entry. All three are load-bearing: an
/// id is injected into card source as a bare name and evaluated by
/// `new Function`, so it must be a JS identifier; ids are the proxy prefix and
/// namespace, so they must be unique; and `user` is reserved (see
/// [`RESERVED_APPLET_ID`]).
///
/// Step ids are deliberately not consulted — an applet writes no artifacts.
fn accept_applets(
    candidates: Vec<Candidate<AppletEntry>>,
    packages: &BTreeMap<&str, String>,
    text: Option<&str>,
) -> (Vec<AppletEntry>, Vec<Diagnostic>) {
    let mut accepted = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for c in candidates {
        let id = c.entry.id.clone();
        if !is_js_identifier(&id) || id.chars().any(|c| c.is_ascii_uppercase()) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "id {id:?} must be a JavaScript identifier in lower case (letters, digits, \
                     _ or $, not starting with a digit): it is injected into card source as a \
                     bare name, and names the folder `system/frontend/<id>/`"
                ),
            ));
            continue;
        }
        if id == RESERVED_APPLET_ID {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "id {RESERVED_APPLET_ID:?} is reserved: it names the namespace for \
                     components the user (or an agent) authors, which the app owns and \
                     never overwrites. Pick another id."
                ),
            ));
            continue;
        }
        if !seen.insert(id.clone()) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("id"),
                    format!("duplicate id {id:?}"),
                )
                .with_help("the earlier entry keeps this id; give this one another"),
            );
            continue;
        }
        if c.entry.command.trim().is_empty() {
            diags.push(c.diag(Severity::Rejected, text, Some("command"), "empty command"));
            continue;
        }
        if let Some(why) = unlisted_package(&c.entry.command, packages) {
            diags.push(c.diag(Severity::Rejected, text, Some("command"), why));
            continue;
        }
        accepted.push(c.entry);
    }
    (accepted, diags)
}

fn complained_about(message: &str) -> Option<&str> {
    message.split('`').nth(1)
}

/// Narrow a diagnostic about an entry to the one key it is about.
///
/// `toml::Value::try_into` reports no span, so we start from the entry header
/// and look for the key serde named. Falls back to that header whenever the
/// search doesn't hold, so this can improve a location and never invent one.
fn key_span(text: &str, header: std::ops::Range<usize>, key: &str) -> std::ops::Range<usize> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return header;
    }
    let mut at = header.end;
    for line in text[header.end..].split_inclusive('\n') {
        let line_start = at;
        at += line.len();
        let trimmed = line.trim_start();
        // A table header ends this entry's body, which keeps the search out of
        // a following `[steps.params]` where an arbitrary key is legal.
        if trimmed.starts_with('[') {
            break;
        }
        let Some(after) = trimmed.strip_prefix(key) else {
            continue;
        };
        // `title = …`, not `titlebar = …`.
        if after.trim_start().starts_with('=') {
            let indent = line.len() - trimmed.len();
            let pos = line_start + indent;
            return pos..pos + key.len();
        }
    }
    header
}

/// What the entry-level pass produced.
struct Entries {
    cfg: DagConfig,
    specs: Vec<StepSpec>,
    lock_specs: Vec<crate::supervisor::locks::LockSpec>,
    /// step id → byte range of the `[[steps]]` header. Includes dropped
    /// steps: a graph diagnostic naming one still wants somewhere to point.
    spans: BTreeMap<String, std::ops::Range<usize>>,
    /// The step ids this pass threw out. Handed to graph assembly so a step
    /// whose input names one of them is told its input was *dropped* rather
    /// than that it never existed — different sentences, different entries
    /// to fix.
    dropped: BTreeSet<String>,
    diagnostics: Vec<Diagnostic>,
}

/// Deserialize one `[[…]]` array entry by entry, so one bad key costs one
/// entry. The id is read off the raw value rather than the deserialized
/// entry, because a rejected entry still has to be nameable.
fn deserialize_each<T: for<'de> Deserialize<'de>>(
    text: &str,
    raw: Vec<toml::Spanned<toml::Value>>,
    make_ref: fn(usize, Option<String>) -> EntryRef,
    id_of: fn(&toml::Value) -> Option<String>,
    diags: &mut Vec<Diagnostic>,
    mut on_raw: impl FnMut(&toml::Value, std::ops::Range<usize>),
) -> Vec<Candidate<T>> {
    let mut out = Vec::with_capacity(raw.len());
    for (i, spanned) in raw.into_iter().enumerate() {
        let span = spanned.span();
        let value = spanned.into_inner();
        on_raw(&value, span.clone());
        let id = id_of(&value);
        let reference = make_ref(i, id);
        match value.try_into::<T>() {
            Ok(entry) => out.push(Candidate {
                entry,
                reference,
                span: Some(span),
            }),
            Err(e) => {
                let message = e.message().trim().to_string();
                let at = match complained_about(&message) {
                    Some(key) => key_span(text, span, key),
                    None => span,
                };
                diags.push(
                    Diagnostic::new(Severity::Rejected, message)
                        .at_entry(reference)
                        .at_span(text, at),
                )
            }
        }
    }
    out
}

/// Deserialize every entry on its own and apply every rule that does not
/// need the graph.
fn entries_of(text: &str) -> Entries {
    let raw: RawConfig = match toml::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            let mut d = Diagnostic::fatal(e.message().trim().to_string()).with_help(
                "this file is not a config — nothing in it could be read. Fix the syntax and \
                 the rest will be checked.",
            );
            if let Some(span) = e.span() {
                d = d.at_span(text, span);
            }
            return Entries {
                cfg: DagConfig::empty(),
                specs: Vec::new(),
                lock_specs: crate::supervisor::locks::defaults(),
                spans: BTreeMap::new(),
                dropped: BTreeSet::new(),
                diagnostics: vec![d],
            };
        }
    };

    let mut diags = Vec::new();
    let mut spans: BTreeMap<String, std::ops::Range<usize>> = BTreeMap::new();
    let mut named: BTreeSet<String> = BTreeSet::new();

    for (key, value) in [
        ("checkpoint_cadence", &raw.checkpoint_cadence),
        ("render_pages", &raw.render_pages),
        ("embed_contexts", &raw.embed_contexts),
    ] {
        if let Some(v) = value {
            diags.push(
                Diagnostic::new(
                    Severity::Rejected,
                    format!(
                        "`{key}` is the shape from before settings a step program reads \
                         moved into `[env]`; it is not used"
                    ),
                )
                .with_help(RETIRED_SHAPE_HELP)
                .at_span(text, v.span()),
            );
        }
    }
    for name in raw.env.keys() {
        if !valid_env_name(name) {
            diags.push(Diagnostic::new(
                Severity::Rejected,
                format!(
                    "[env] {name:?} is not a variable name: letters, digits and `_`, not \
                     starting with a digit; it is not handed to any step"
                ),
            ));
        }
    }
    let env: BTreeMap<String, String> = raw
        .env
        .into_iter()
        .filter(|(k, _)| valid_env_name(k))
        .collect();

    let id_key = |v: &toml::Value| v.get("id").and_then(v_str);
    let locks = deserialize_each(
        text,
        raw.locks,
        EntryRef::lock,
        |v: &toml::Value| v.get("name").and_then(v_str),
        &mut diags,
        |_, _| {},
    );
    let groups = deserialize_each(
        text,
        raw.groups,
        EntryRef::group,
        id_key,
        &mut diags,
        |_, _| {},
    );
    let steps = deserialize_each(
        text,
        raw.steps,
        EntryRef::step,
        raw_step_id,
        &mut diags,
        |v, span| {
            if let Some(id) = raw_step_id(v) {
                named.extend(group_of(&id).map(str::to_string));
                // First spelling wins, matching `accept_steps`.
                spans.entry(id).or_insert(span);
            }
        },
    );
    let applets = deserialize_each(
        text,
        raw.applets,
        EntryRef::applet,
        id_key,
        &mut diags,
        |v, _| {
            named.extend(v.get("group").and_then(v_str));
        },
    );
    let packages = deserialize_each(
        text,
        raw.packages,
        EntryRef::package,
        id_key,
        &mut diags,
        |_, _| {},
    );

    let accepted = accept_entries(
        Candidates {
            programs: raw.programs.clone(),
            locks,
            packages,
            groups,
            steps,
            applets,
            named,
        },
        Some(text),
    );
    diags.extend(accepted.diagnostics);

    let mut steps = Vec::with_capacity(accepted.steps.len());
    let mut specs = Vec::with_capacity(accepted.steps.len());
    for (entry, spec) in accepted.steps {
        steps.push(entry);
        specs.push(spec);
    }

    // Read off the diagnostics rather than tracked as entries are dropped, so
    // a rule added above cannot forget to report here. Steps only: the set
    // answers "did an *input* name something that was thrown out".
    let kept: BTreeSet<&str> = steps.iter().map(|e| e.id.as_str()).collect();
    let dropped: BTreeSet<String> = diags
        .iter()
        .filter(|d| d.severity.drops_the_entry())
        .filter(|d| d.entry.as_ref().is_some_and(|e| e.kind == EntryKind::Step))
        .filter_map(|d| d.id())
        .filter(|id| !kept.contains(id))
        .map(str::to_string)
        .collect();

    Entries {
        cfg: DagConfig {
            data_root: raw.data_root,
            binary_dir: raw.binary_dir,
            groups: accepted.groups,
            steps,
            applets: accepted.applets,
            packages: accepted.packages,
            locks: accepted.locks,
            programs: raw.programs,
            env,
            run_history: raw.run_history,
            log_level: raw.log_level,
            disk_space: raw.disk_space,
        },
        specs,
        lock_specs: accepted.lock_specs,
        spans,
        dropped,
        diagnostics: diags,
    }
}

/// Whether `name` can be an environment variable a config sets.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn v_str(v: &toml::Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Parse config text, keeping every entry that loads; only a file-level
/// problem leaves nothing to salvage. Builds no graph, so `inputs` are
/// unresolved — [`check_text`] is the whole answer.
pub fn parse_graded(text: &str) -> (DagConfig, Vec<Diagnostic>) {
    let e = entries_of(text);
    (e.cfg, e.diagnostics)
}

/// Everything the loader can say about one config text.
pub struct ConfigCheck {
    /// The exact bytes checked. Diagnostics carry byte spans into it, so
    /// keeping it here means no caller has to carry the two together.
    pub text: String,
    /// The entries that survived. A valid config: a caller may use it without
    /// looking at the diagnostics at all.
    pub cfg: DagConfig,
    /// The graph built from `cfg`. Empty when the file is not a config.
    pub graph: Graph,
    /// One per problem, in file order — the order someone fixing them reads.
    /// Sort by [`Severity`] for worst-first; its `Ord` is blast radius.
    pub diagnostics: Vec<Diagnostic>,
}

impl ConfigCheck {
    /// Nothing loaded: the file is not a config. The one state that should
    /// stop the whole app.
    pub fn is_fatal(&self) -> bool {
        self.worst() == Some(Severity::Fatal)
    }

    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Whether every entry reached the graph. A warning leaves this true:
    /// it is advice about a config that runs exactly as written.
    pub fn nothing_dropped(&self) -> bool {
        self.dropped() == 0
    }

    pub fn worst(&self) -> Option<Severity> {
        self.diagnostics.iter().map(|d| d.severity).max()
    }

    /// How many entries did not reach the graph.
    pub fn dropped(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity.drops_the_entry())
            .count()
    }

    pub fn render(&self, path: &Path) -> String {
        self.diagnostics
            .iter()
            .map(|d| d.render(path, &self.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The whole chokepoint: config text in, the graph that will actually run
/// out, plus one diagnostic per entry that will not. Every entry point calls
/// this, because a rule enforced in one caller is a rule a hand-edit
/// silently breaks.
pub fn check_text(text: &str) -> ConfigCheck {
    let mut entries = entries_of(text);
    let (mut graph, mut graph_diags) =
        Graph::build_graded(std::mem::take(&mut entries.specs), &entries.dropped);
    graph.locks = std::mem::take(&mut entries.lock_specs);
    graph.disk_floor = Some(entries.cfg.disk_space.unwrap_or_default());

    // Graph assembly drops more than the entry pass could see — a step whose
    // input names nothing, a ring — so narrow the surviving config to what the
    // graph kept. Otherwise `cfg` and `graph` disagree about what survived.
    entries
        .cfg
        .steps
        .retain(|s| graph.by_id.contains_key(&s.id));

    // Graph diagnostics know a step id but not where it sits in the file, so
    // lend them the location here rather than threading the text through
    // graph assembly.
    for d in &mut graph_diags {
        if let Some(span) = d.id().and_then(|id| entries.spans.get(id)).cloned() {
            d.set_span(text, span);
        }
    }
    entries.diagnostics.extend(graph_diags);

    ConfigCheck {
        text: text.to_string(),
        cfg: entries.cfg,
        graph,
        diagnostics: entries.diagnostics,
    }
}

impl DagConfig {
    /// The `RUST_LOG`-grammar filter this config asks for: its
    /// `log_level`, else the default level, over the library caps.
    pub fn log_filter(&self) -> String {
        datalib_runs::filter_at(
            self.log_level
                .map(datalib_runs::LogLevel::as_str)
                .unwrap_or(datalib_runs::DEFAULT_LEVEL),
        )
    }

    /// What a fatal diagnostic leaves behind. Deliberately not `Default`:
    /// "empty" here is a failure state and should read as one.
    fn empty() -> Self {
        DagConfig {
            data_root: None,
            binary_dir: None,
            groups: Vec::new(),
            steps: Vec::new(),
            applets: Vec::new(),
            packages: Vec::new(),
            locks: Vec::new(),
            programs: Vec::new(),
            env: BTreeMap::new(),
            run_history: None,
            log_level: None,
            disk_space: None,
        }
    }
}

#[cfg(test)]
mod disk_floor_tests {
    use super::check_text;
    use crate::disk_space::DiskFloor;

    /// The floor is written the way a person says a size, reaches the
    /// graph the loop reads (so an edit mid-sync is taken on), and is on
    /// at its defaults in a config that says nothing of it.
    #[test]
    fn the_free_space_floor_reads_human_units_and_reaches_the_graph() {
        let check = check_text(
            "[disk_space]\npause_below_bytes = \"5000 MB\"\nresume_at_bytes = \"8 GB\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let floor = DiskFloor {
            pause_below: 5_000_000_000,
            resume_at: 8_000_000_000,
        };
        assert_eq!(check.cfg.disk_space, Some(floor));
        assert_eq!(check.graph.disk_floor, Some(floor));
        assert_eq!(check_text("").graph.disk_floor, Some(DiskFloor::default()));
        assert!(check_text("[disk_space]\npause_below_bytes = \"10 XB\"\n").is_fatal());
        assert!(check_text(
            "[disk_space]\npause_below_bytes = \"20 GB\"\nresume_at_bytes = \"1 GB\"\n"
        )
        .is_fatal());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `code_version` has to survive the trip from TOML into the spec,
    /// or the fingerprint never sees it and bumping it silently does
    /// nothing. The scheduler tests use the builder, not this path.
    #[test]
    fn code_version_reaches_the_spec_and_moves_the_fingerprint() {
        let with = |line: &str| {
            let cfg: DagConfig = toml::from_str(&format!(
                r#"
                [[steps]]
                id = "slack/render_markdown"
                inputs = ["slack/ingest"]
                command = "render-slack"
                {line}
                "#
            ))
            .expect("parse");
            to_specs(&cfg).expect("to_specs").remove(0)
        };

        let none = with("");
        let v1 = with(r#"code_version = "v1""#);
        let v2 = with(r#"code_version = "v2""#);

        assert_eq!(none.code_version, None);
        assert_eq!(v1.code_version.as_deref(), Some("v1"));
        assert_ne!(
            v1.fingerprint_material(),
            v2.fingerprint_material(),
            "a bumped code_version must change what the step fingerprints to"
        );
        assert_ne!(none.fingerprint_material(), v1.fingerprint_material());
    }

    /// Editing `params` changes the argv the runner executes, which is
    /// what makes a config edit re-run the step.
    #[test]
    fn params_edit_moves_the_fingerprint() {
        let with = |since: &str| {
            let cfg: DagConfig = toml::from_str(&format!(
                r#"
                [[steps]]
                id = "slack/ingest"
                command = "fetch-slack"
                params.api = {{ since = "{since}" }}
                "#
            ))
            .expect("parse");
            to_specs(&cfg).expect("to_specs").remove(0)
        };
        assert_ne!(
            with("2026-06-15").fingerprint_material(),
            with("2020-01-01").fingerprint_material()
        );
    }

    /// A group is display metadata, and its `type` is opaque to the
    /// runner: editing any of the three re-runs nothing.
    #[test]
    fn a_groups_type_name_and_description_move_no_fingerprint() {
        let with = |group_lines: &str| {
            let cfg: DagConfig = toml::from_str(&format!(
                r#"
                [[groups]]
                id = "mail"
                {group_lines}

                [[groups]]
                id = "unified_document_index"

                [[steps]]
                id = "mail/ingest"
                command = "my-fetcher"

                [[steps]]
                id = "unified_document_index/qmd_index"
                command = "my-indexer"
                inputs = ["mail/ingest"]
                "#
            ))
            .expect("parse");
            to_specs(&cfg).expect("to_specs")
        };
        let bare = with("");
        for lines in [
            r#"type = "email""#,
            r#"type = "slack""#,
            r#"name = "Fastmail""#,
            r#"description = "Fastmail, mostly receipts""#,
        ] {
            let edited = with(lines);
            for i in 0..2 {
                assert_eq!(
                    bare[i].fingerprint_material(),
                    edited[i].fingerprint_material(),
                    "{lines}"
                );
            }
        }
    }

    #[test]
    fn command_splits_shell_style() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "custom/out"
            command = """sh -c 'echo "hi there" > custom/out/x.txt'"""
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        match &specs[0].run {
            StepRun::Subprocess { argv, .. } => {
                assert_eq!(argv, &["sh", "-c", r#"echo "hi there" > custom/out/x.txt"#]);
            }
            other => panic!("expected subprocess, got {other:?}"),
        }
    }

    #[test]
    fn bad_commands_are_rejected() {
        let cfg: DagConfig =
            toml::from_str(r#"steps = [{id = "x/raw", command = "unbalanced '"}]"#).unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("unbalanced quoting"), "{err}");

        let cfg: DagConfig = toml::from_str(r#"steps = [{id = "x/raw", command = ""}]"#).unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("empty command"), "{err}");
    }

    /// The whole point of `name`: it is not part of what the step is,
    /// so renaming one cannot make it stale. If this ever fails, every
    /// rename in the UI silently re-runs a download.
    #[test]
    fn a_name_changes_neither_argv_nor_fingerprint() {
        let bare: DagConfig =
            toml::from_str(r#"steps = [{id = "slack/ingest", command = "fetch-slack"}]"#).unwrap();
        let named: DagConfig = toml::from_str(
            r#"steps = [{id = "slack/ingest", name = "Work Slack", command = "fetch-slack"}]"#,
        )
        .unwrap();
        assert_eq!(named.steps[0].name.as_deref(), Some("Work Slack"));

        let bare = to_specs(&bare).unwrap();
        let named = to_specs(&named).unwrap();
        match (&bare[0].run, &named[0].run) {
            (StepRun::Subprocess { argv: a, .. }, StepRun::Subprocess { argv: b, .. }) => {
                assert_eq!(a, b, "a name must not reach the child's argv")
            }
            other => panic!("expected subprocesses, got {other:?}"),
        }
        assert_eq!(
            bare[0].fingerprint_material(),
            named[0].fingerprint_material()
        );
    }

    #[test]
    fn rejects_duplicate_step_ids() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/raw", command = "a"},
                 {id = "slack/raw", command = "b"},
               ]"#,
        )
        .unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate id"), "{err}");
        // Same rule, said the other way: two steps writing one tree.
        assert!(err.contains("tree it writes"), "{err}");
    }

    /// Two sources of the same type, which is what "+ Data Source"
    /// produces the second time someone connects a Slack workspace.
    #[test]
    fn distinct_ids_are_fine() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/raw", command = "a"},
                 {id = "slack-2/raw", command = "b"},
               ]"#,
        )
        .unwrap();
        to_specs(&cfg).expect("distinct ids must pass");
    }

    /// Sharing a stem is not sharing a tree: a download and a render
    /// step sit side by side under one directory and are two distinct
    /// steps, which is the whole layout.
    #[test]
    fn a_shared_stem_is_not_a_collision() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/raw", command = "a"},
                 {id = "slack/rendered_md", command = "b", inputs = ["slack/raw"]},
               ]"#,
        )
        .unwrap();
        let specs = to_specs(&cfg).expect("siblings must pass");
        crate::Graph::build(specs).expect("siblings must graph");
    }

    /// An id becomes a directory, so it has to be able to be one.
    #[test]
    fn rejects_ids_that_cannot_be_directories() {
        for id in ["", "a//b", "a/../b", "./a", "-lead", "a/b c", "star*"] {
            let cfg: DagConfig =
                toml::from_str(&format!(r#"steps = [{{id = "{id}", command = "a"}}]"#)).unwrap();
            assert!(
                to_specs(&cfg).is_err(),
                "{id:?} should be rejected as a step id"
            );
        }
        for id in ["a", "a/b", "a/b/c", "a.b/c_d-e", "slack-2/rendered_md"] {
            let cfg: DagConfig =
                toml::from_str(&format!(r#"steps = [{{id = "{id}", command = "a"}}]"#)).unwrap();
            to_specs(&cfg).unwrap_or_else(|e| panic!("{id:?} should be a valid step id: {e}"));
        }
    }

    /// An input is a step id. Naming a directory instead is the
    /// mistake worth a pointed message, since it is what every
    /// pre-phase-2 config does.
    #[test]
    fn rejects_an_input_that_names_no_step() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [{id = "slack/rendered_md", command = "a", inputs = ["slack/raw"]}]"#,
        )
        .unwrap();
        let specs = to_specs(&cfg).expect("to_specs does not resolve inputs");
        let err = crate::Graph::build(specs).unwrap_err().to_string();
        assert!(err.contains("names no declared step"), "{err}");
    }

    #[test]
    fn rejects_ids_under_a_reserved_dir() {
        for id in [
            "system",
            "system/state",
            "system/a/b",
            "datalib_curated",
            "datalib_curated/datalib_contacts",
        ] {
            let cfg: DagConfig =
                toml::from_str(&format!(r#"steps = [{{id = "{id}", command = "a"}}]"#)).unwrap();
            let err = to_specs(&cfg).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{id}: {err}");
        }
    }

    /// An id is a folder name, and a Mac's disk ignores letter case, so
    /// `SYSTEM` there is `system/` and `Email` beside `email` is one
    /// folder with two owners. One spelling per folder: lower case.
    #[test]
    fn ids_that_name_folders_are_lower_case() {
        for id in ["SYSTEM", "System/state", "Email/ingest", "email/Ingest"] {
            let cfg: DagConfig =
                toml::from_str(&format!(r#"steps = [{{id = "{id}", command = "a"}}]"#)).unwrap();
            let err = to_specs(&cfg).unwrap_err().to_string();
            assert!(err.contains("lowercase"), "{id}: {err}");
        }
        for id in ["SYSTEM", "Datalib_Curated", "Email"] {
            assert!(!usable_group_id(id), "{id}");
        }
        let check = check_text(
            "[[groups]]\nid = \"Email\"\n\n\
             [[steps]]\nid = \"Email/raw\"\ncommand = \"x\"\n\n\
             [[groups]]\nid = \"g\"\n\n\
             [[steps]]\nid = \"g/Raw\"\ncommand = \"x\"\n",
        );
        assert!(
            check.cfg.groups.iter().all(|g| g.id == "g"),
            "{:?}",
            check.diagnostics
        );
        assert!(check.cfg.steps.is_empty(), "{:?}", check.diagnostics);
        let applet: DagConfig =
            toml::from_str("[[applets]]\nid = \"User\"\ncommand = \"x\"\n").unwrap();
        let err = validate_applets(&applet).unwrap_err().to_string();
        assert!(err.contains("lower case"), "{err}");
    }

    /// Among steps, the index needs no reserved-name rule: the index
    /// steps' ids *are* those trees, so anything else claiming one is an
    /// ordinary duplicate.
    #[test]
    fn the_index_steps_own_unified_document_index_by_being_it() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/rendered_md", command = "r"},
                 {id = "unified_document_index/grid", command = "a", inputs = ["slack/rendered_md"]},
                 {id = "unified_document_index/qmd", command = "b", inputs = ["slack/rendered_md"]},
               ]"#,
        )
        .unwrap();
        let specs = to_specs(&cfg).expect("index steps must remain valid");
        crate::Graph::build(specs).expect("index steps must graph");

        // And a second claimant is refused as a duplicate, with no
        // special-case list involved.
        let clash: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "unified_document_index/grid", command = "a"},
                 {id = "unified_document_index/grid", command = "b"},
               ]"#,
        )
        .unwrap();
        assert!(to_specs(&clash).is_err());
    }

    /// TOML dates are a distinct scalar type with no JSON counterpart;
    /// they must reach the step as the string the user typed, not as
    /// the `toml` crate's internal `$__toml_private_datetime` wrapper.
    #[test]
    fn toml_datetimes_reach_the_step_as_strings() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "x/raw"
            command = "s"
            params.api = {since = 2026-06-15, at = 2026-06-15T10:30:00Z}
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        let StepRun::Subprocess { params, .. } = &specs[0].run else {
            panic!("expected subprocess");
        };
        let params: serde_json::Value = serde_json::from_str(params.as_deref().unwrap()).unwrap();
        assert_eq!(params["api"]["since"], "2026-06-15");
        assert_eq!(params["api"]["at"], "2026-06-15T10:30:00Z");
    }

    #[test]
    fn binary_dir_resolution_prefers_cli_then_config() {
        let cfg: DagConfig = toml::from_str(r#"binary_dir = "/opt/datalib/bin""#).unwrap();
        assert_eq!(
            resolve_binary_dir(&cfg, None),
            Some(PathBuf::from("/opt/datalib/bin"))
        );
        // CLI override wins, and relative paths are pinned to the
        // runner's cwd (children run with cwd = data_root).
        let got = resolve_binary_dir(&cfg, Some(Path::new("bazel-bin/x"))).unwrap();
        assert_eq!(got, std::env::current_dir().unwrap().join("bazel-bin/x"));

        // No CLI/config → the runner executable's own directory.
        let cfg: DagConfig = toml::from_str("").unwrap();
        let got = resolve_binary_dir(&cfg, None).unwrap();
        assert_eq!(
            got,
            std::env::current_exe().unwrap().parent().unwrap(),
            "default is the running executable's directory"
        );
    }

    #[test]
    fn data_root_defaults_to_config_dir() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("pipeline.toml");
        std::fs::write(&p, "steps = []\n").unwrap();
        let (_cfg, root) = load(&p).unwrap();
        assert_eq!(root, std::fs::canonicalize(td.path()).unwrap());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<DagConfig>("step_bin = \"/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}

#[cfg(test)]
mod applet_tests {
    use super::*;

    /// Deserialization only — deliberately not [`parse`], which now
    /// validates too. These tests are about `validate_applets`, so the
    /// config has to be constructible while still breaking its rules.
    fn cfg(text: &str) -> DagConfig {
        toml::from_str(text).expect("deserialize")
    }

    #[test]
    fn applets_default_to_empty() {
        let c = cfg("data_root = \"/tmp/x\"\n");
        assert!(c.applets.is_empty());
        validate_applets(&c).expect("empty list is valid");
    }

    #[test]
    fn parses_an_applet_with_params() {
        let c = cfg(r#"
[[applets]]
id = "slack_work"
command = "datalib-applet slack"
[applets.params]
tree = "slack_work/rendered_md"
"#);
        assert_eq!(c.applets.len(), 1);
        let a = &c.applets[0];
        assert_eq!(a.id, "slack_work");
        let params = a.params_json().unwrap().expect("params present");
        assert_eq!(params["tree"], "slack_work/rendered_md");
    }

    #[test]
    fn an_applet_may_declare_no_params() {
        let c = cfg("[[applets]]\nid = \"grid\"\ncommand = \"x\"\n");
        assert!(c.applets[0].params_json().unwrap().is_none());
    }

    /// `title` was accepted and read by nothing. Removing it makes a config
    /// that still carries one fail to parse — pinned because that is what a
    /// user upgrading hits, and `deny_unknown_fields` is what makes the
    /// message name the key.
    #[test]
    fn a_leftover_title_is_rejected_by_name() {
        let err = parse("[[applets]]\nid = \"grid\"\ntitle = \"Grid\"\ncommand = \"x\"\n")
            .expect_err("title is no longer a field");
        let msg = err.to_string();
        assert!(msg.contains("title"), "{msg}");
    }

    /// The id reaches card source as a bare identifier, so a dotted or
    /// digit-leading id would blow up inside `new Function` at render
    /// time rather than at config load.
    #[test]
    fn rejects_ids_that_are_not_js_identifiers() {
        for bad in ["slack.work", "2fa", "has-dash", "", "with space", "class"] {
            let c = cfg(&format!("[[applets]]\nid = \"{bad}\"\ncommand = \"x\"\n"));
            assert!(
                validate_applets(&c).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_identifiers() {
        for good in ["grid", "slack_work", "_priv", "$x", "a1"] {
            let c = cfg(&format!("[[applets]]\nid = \"{good}\"\ncommand = \"x\"\n"));
            validate_applets(&c).unwrap_or_else(|e| panic!("{good:?} rejected: {e}"));
        }
    }

    /// `user` is where hand-authored components live and a refresh
    /// wipes every applet namespace, so letting an applet claim it
    /// would delete the user's own work.
    #[test]
    fn rejects_the_reserved_user_id() {
        let c = cfg("[[applets]]\nid = \"user\"\ncommand = \"x\"\n");
        let err = validate_applets(&c).expect_err("\"user\" must be refused");
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let c = cfg(r#"
[[applets]]
id = "slack"
command = "a"

[[applets]]
id = "slack"
command = "b"
"#);
        let err = validate_applets(&c).expect_err("duplicate must fail");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    /// Two instances of one command is the case the whole design is
    /// built around; it must parse without complaint.
    #[test]
    fn two_instances_of_one_command_are_fine() {
        let c = cfg(r#"
[[applets]]
id = "a"
command = "datalib-applet slack"
[applets.params]
tree = "a/rendered_md"

[[applets]]
id = "b"
command = "datalib-applet slack"
[applets.params]
tree = "b/rendered_md"
"#);
        validate_applets(&c).expect("distinct ids, same command");
        assert_eq!(c.applets.len(), 2);
    }
}

// --- The graded loader -----------------------------------------------------

#[cfg(test)]
mod graded_tests {
    use super::*;

    fn sev_of(diags: &[Diagnostic], id: &str) -> Option<Severity> {
        diags
            .iter()
            .find(|d| d.id() == Some(id))
            .map(|d| d.severity)
    }

    fn ids(check: &ConfigCheck) -> Vec<&str> {
        let mut v: Vec<&str> = check.cfg.steps.iter().map(|s| s.id.as_str()).collect();
        v.sort();
        v
    }

    /// The headline of #209: one stray key in one step used to cost the
    /// grid, search, the document view and every applet. It now costs
    /// that step.
    #[test]
    fn one_bad_step_costs_only_that_step() {
        let check = check_text(
            r#"
[[steps]]
id = "slack/ingest"
command = "fetch-slack"
title = "Work Slack"

[[steps]]
id = "pdfs/ingest"
command = "fetch-pdfs"

[[steps]]
id = "unified_document_index/grid_index"
command = "index-it"
inputs = ["pdfs/ingest"]

[[applets]]
id = "unified_document_index"
command = "datalib-applet unified_document_index"
"#,
        );
        // The broken step is gone and named; everything else loaded.
        assert_eq!(
            ids(&check),
            vec!["pdfs/ingest", "unified_document_index/grid_index"]
        );
        assert_eq!(check.cfg.applets.len(), 1, "the applet must survive");
        assert_eq!(
            sev_of(&check.diagnostics, "slack/ingest"),
            Some(Severity::Rejected)
        );
        assert!(!check.is_fatal());
        assert_eq!(check.dropped(), 1);

        // …and the message names the key, which is what makes it
        // fixable without reading the schema.
        let d = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("slack/ingest"))
            .unwrap();
        assert!(d.message.contains("title"), "{}", d.message);
        assert_eq!(
            d.line,
            Some(5),
            "the `title =` line itself, not the entry header"
        );
    }

    /// Two steps writing under one tree is two writers on one `.doltlite_db`,
    /// which commit each other's in-flight rows with no failed step and no log
    /// line. Checked both ways round, since either order can be declared first.
    #[test]
    fn nested_ids_are_two_writers_on_one_tree() {
        for (first, second) in [
            ("unified_document_index", "unified_document_index/grid"),
            ("unified_document_index/grid", "unified_document_index"),
        ] {
            let check = check_text(&format!(
                "[[steps]]\nid = \"{first}\"\ncommand = \"a\"\n\n\
                 [[steps]]\nid = \"{second}\"\ncommand = \"b\"\n"
            ));
            assert_eq!(
                ids(&check),
                vec![first],
                "the later of {first:?} / {second:?} must be dropped"
            );
            let d = check
                .diagnostics
                .iter()
                .find(|d| d.id() == Some(second))
                .unwrap_or_else(|| panic!("no diagnostic for {second:?}: {:?}", check.diagnostics));
            assert_eq!(d.severity, Severity::Rejected);
            // Both ids, so the reader can see the pair that collides.
            assert!(d.message.contains(first), "{}", d.message);
            assert!(d.message.contains(second), "{}", d.message);
        }
    }

    /// Siblings are the whole layout and must stay legal — a download
    /// and a render step under one stem write two different trees.
    #[test]
    fn siblings_under_one_stem_are_not_nested() {
        let check = check_text(
            "[[steps]]\nid = \"slack/raw\"\ncommand = \"a\"\n\n\
             [[steps]]\nid = \"slack/rendered_md\"\ncommand = \"b\"\ninputs = [\"slack/raw\"]\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
    }

    /// Applet ids and step ids are separate namespaces, and the scaffold
    /// depends on it: `unified_document_index` the applet sits beside
    /// `unified_document_index/grid_index` the step.
    #[test]
    fn the_scaffold_shape_loads_clean() {
        let check = check_text(
            r#"
[[groups]]
id = "unified_document_index"

[[steps]]
id = "unified_document_index/grid_index"
command = "index-grid"

[[steps]]
id = "unified_document_index/qmd_aggregator"
command = "index-qmd"

[[applets]]
group = "unified_document_index"
id = "unified_document_index"
command = "datalib-applet unified_document_index"
"#,
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert_eq!(check.graph.steps.len(), 2);
    }

    /// Malformed TOML is the one shape with nothing to salvage.
    #[test]
    fn malformed_toml_is_fatal_and_nothing_loads() {
        let check = check_text("[[steps]]\nid = = \n");
        assert!(check.is_fatal());
        assert!(check.cfg.steps.is_empty());
        assert!(check.graph.steps.is_empty());
        assert_eq!(check.diagnostics.len(), 1, "one fatal, not a pile");
        assert_eq!(check.diagnostics[0].line, Some(2));
    }

    /// An unknown key at the *top* level is a statement about the file,
    /// not about an entry — there is no smaller thing to drop, so it
    /// stays fatal.
    #[test]
    fn an_unknown_top_level_key_is_fatal() {
        let check = check_text("stpes = 1\n[[steps]]\nid = \"a\"\ncommand = \"x\"\n");
        assert!(check.is_fatal(), "{:?}", check.diagnostics);
        assert!(
            check.diagnostics[0].message.contains("data_root"),
            "the message should list the keys that are allowed: {}",
            check.diagnostics[0].message
        );
    }

    /// The issue's "input names no declared step" row: that step is
    /// blocked, its dependents with it, and everything else runs.
    #[test]
    fn a_dangling_input_blocks_its_step_and_its_dependents() {
        let check = check_text(
            r#"
[[steps]]
id = "pdfs/raw"
command = "a"

[[steps]]
id = "slack/rendered_md"
command = "b"
inputs = ["slack/raw"]

[[steps]]
id = "unified_document_index/grid"
command = "c"
inputs = ["slack/rendered_md", "pdfs/raw"]
"#,
        );
        assert_eq!(ids(&check), vec!["pdfs/raw"]);
        assert_eq!(
            sev_of(&check.diagnostics, "slack/rendered_md"),
            Some(Severity::Blocked)
        );
        assert_eq!(
            sev_of(&check.diagnostics, "unified_document_index/grid"),
            Some(Severity::Blocked)
        );

        // The step that named a missing id is told what does exist…
        let dangling = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("slack/rendered_md"))
            .unwrap();
        assert!(
            dangling.message.contains("names no declared step"),
            "{dangling:?}"
        );
        let help = dangling.help.as_deref().unwrap_or_default();
        assert!(
            help.contains("pdfs/raw"),
            "should list the declared steps: {help}"
        );
        assert!(help.contains("path = "), "{help}");

        // …and the one that merely hangs off it is told the fix is
        // elsewhere, so nobody goes editing the wrong entry.
        let cascaded = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("unified_document_index/grid"))
            .unwrap();
        assert!(
            cascaded.message.contains("was itself dropped"),
            "{cascaded:?}"
        );
        assert!(
            cascaded
                .help
                .as_deref()
                .unwrap()
                .contains("slack/rendered_md"),
            "the help must name the entry that actually needs fixing: {cascaded:?}"
        );
    }

    /// The commonest cascade: a render step whose fetch step was rejected for
    /// a bad key. The fetch step never reaches graph assembly, so without being
    /// told what the entry pass threw out the graph would blame the render step.
    #[test]
    fn a_step_whose_input_was_rejected_is_told_where_the_fix_is() {
        let check = check_text(
            r#"
[[steps]]
id = "slack/raw"
command = "a"
title = "Work Slack"

[[steps]]
id = "slack/rendered_md"
command = "b"
inputs = ["slack/raw"]
"#,
        );
        assert!(ids(&check).is_empty());
        let d = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("slack/rendered_md"))
            .unwrap();
        assert_eq!(d.severity, Severity::Blocked);
        assert!(
            d.message.contains("was itself dropped"),
            "the input exists in the file — saying it was never declared sends the \
             reader to the wrong entry: {d:?}"
        );
        assert!(!d.message.contains("names no declared step"), "{d:?}");
        assert!(d.help.as_deref().unwrap().contains("slack/raw"), "{d:?}");
    }

    /// A cycle blocks the ring; the rest of the config still runs. What
    /// merely *hangs off* the ring is told so, rather than being
    /// accused of being in it.
    #[test]
    fn a_cycle_blocks_the_ring_and_what_hangs_off_it() {
        let check = check_text(
            r#"
[[steps]]
id = "fine/raw"
command = "ok"

[[steps]]
id = "a"
command = "x"
inputs = ["b"]

[[steps]]
id = "b"
command = "y"
inputs = ["a"]

[[steps]]
id = "downstream"
command = "z"
inputs = ["a"]
"#,
        );
        assert_eq!(ids(&check), vec!["fine/raw"]);
        for id in ["a", "b", "downstream"] {
            assert_eq!(
                sev_of(&check.diagnostics, id),
                Some(Severity::Blocked),
                "{id}"
            );
        }
        let ring = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("a"))
            .unwrap();
        assert!(
            ring.message.contains("is in a dependency cycle"),
            "{ring:?}"
        );
        let tail = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("downstream"))
            .unwrap();
        assert!(
            tail.message.contains("downstream of a dependency cycle"),
            "a step below a cycle is not in it: {tail:?}"
        );
    }

    /// Diagnostics raised during graph assembly know a step id but not
    /// where it sits in the file. The loader lends them the location,
    /// or the UI has nothing to jump to.
    #[test]
    fn graph_diagnostics_get_a_line_from_the_loader() {
        let check = check_text(
            "[[steps]]\nid = \"ok/raw\"\ncommand = \"a\"\n\n\
             [[steps]]\nid = \"bad/rendered_md\"\ncommand = \"b\"\ninputs = [\"nope\"]\n",
        );
        let d = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("bad/rendered_md"))
            .unwrap();
        assert_eq!(d.line, Some(5), "the second [[steps]] header");
        assert!(d.span.is_some(), "the UI editor selects the span");
    }

    /// Two entries claiming one id: the first keeps it, so a config's
    /// meaning does not depend on which duplicate the loader happened
    /// to visit last.
    #[test]
    fn the_first_of_two_duplicate_ids_wins() {
        let check = check_text(
            "[[steps]]\nid = \"x/raw\"\ncommand = \"first\"\n\n\
             [[steps]]\nid = \"x/raw\"\ncommand = \"second\"\n",
        );
        assert_eq!(check.cfg.steps.len(), 1);
        assert_eq!(check.cfg.steps[0].command, "first");
        assert_eq!(
            check.diagnostics[0].line,
            Some(6),
            "the `id =` line of the later entry — the loser, and the line to edit"
        );
    }

    /// A file with no problems produces no diagnostics at all — the
    /// state every other assertion here is measured against.
    #[test]
    fn a_good_config_says_nothing() {
        let check = check_text(
            "[[steps]]\nid = \"a/raw\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"a/rendered_md\"\ncommand = \"y\"\ninputs = [\"a/raw\"]\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert_eq!(check.worst(), None);
        assert_eq!(check.dropped(), 0);
        assert_eq!(check.graph.topo.len(), 2);
    }

    /// A bad applet costs the applet, not the pipeline — and the other
    /// way round. They are declared in one file and that is the only
    /// thing they share.
    #[test]
    fn a_bad_applet_and_a_bad_step_do_not_touch_each_other() {
        let check = check_text(
            r#"
[[steps]]
id = "good/raw"
command = "a"

[[steps]]
id = "system/sneaky"
command = "b"

[[applets]]
id = "unified_document_index"
command = "datalib-applet unified_document_index"

[[applets]]
id = "2fa"
command = "x"
"#,
        );
        assert_eq!(ids(&check), vec!["good/raw"]);
        assert_eq!(check.cfg.applets.len(), 1);
        assert_eq!(check.cfg.applets[0].id, "unified_document_index");
        assert_eq!(
            sev_of(&check.diagnostics, "system/sneaky"),
            Some(Severity::Rejected)
        );
        assert_eq!(sev_of(&check.diagnostics, "2fa"), Some(Severity::Rejected));
    }

    /// `is_toml` answers the file-level question only. A config with a
    /// real problem in it is still a TOML config, which is what keeps
    /// `datalib-migrate-config` from re-converting one.
    #[test]
    fn is_toml_ignores_everything_but_the_syntax() {
        assert!(is_toml("[[steps]]\nid = \"a\"\ncommand = \"x\"\n"));
        assert!(
            is_toml(
                "[[steps]]\nid = \"a\"\ncommand = \"x\"\n[[steps]]\nid = \"a\"\ncommand = \"y\"\n"
            ),
            "a duplicate id is a problem, not a reason to call this YAML"
        );
        assert!(!is_toml("sources:\n  - name: slack\n"));
    }

    /// A diagnostic points at the key it is about, not at the entry it is in.
    /// `toml::Value::try_into` reports no span at all, so the location is
    /// found rather than given, and falls back to the entry header.
    #[test]
    fn a_diagnostic_points_at_the_offending_key() {
        let text = "[[steps]]\nid = \"a/raw\"\ncommand = \"x\"\ntitle = \"nope\"\n";
        let check = check_text(text);
        let d = &check.diagnostics[0];
        let (start, end) = d.span.unwrap();
        assert_eq!(&text[start..end], "title");
        assert_eq!(d.line, Some(4));

        // A key inside `params` is legal, so a same-named key there is
        // never what a complaint is about. The search stops at the
        // sub-table header rather than reaching in.
        let with_params =
            "[[steps]]\nid = \"a/raw\"\ncommand = \"x\"\nbogus = 1\n[steps.params]\nbogus = 2\n";
        let d2 = &check_text(with_params).diagnostics[0];
        assert_eq!(
            d2.line,
            Some(4),
            "the entry's own key, not the one in params"
        );
    }

    /// The strict door and the graded one enforce one rule set: what
    /// `parse` rejects is exactly what the graded loader drops.
    #[test]
    fn strict_parse_rejects_exactly_what_the_graded_loader_drops() {
        for text in [
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\ntitle = 1\n",
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\n[[steps]]\nid = \"a\"\ncommand = \"y\"\n",
            "[[steps]]\nid = \"system\"\ncommand = \"x\"\n",
            "[[applets]]\nid = \"user\"\ncommand = \"x\"\n",
            "[[groups]]\nid = \"a/b\"\n",
            "[[steps]]\ngroup = \"nope\"\nfunction = \"raw\"\ncommand = \"x\"\n",
            "nope = 1\n",
        ] {
            let (_, diags) = parse_graded(text);
            assert!(
                diags.iter().any(|d| d.severity.drops_the_entry()),
                "graded dropped nothing for {text:?}"
            );
            assert!(parse(text).is_err(), "strict accepted {text:?}");
        }
        // A warning drops nothing, so the strict door lets it through: what
        // it returns is exactly what the graded loader would have run.
        let warned = "[[groups]]\nid = \"lonely\"\n";
        let (_, diags) = parse_graded(warned);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert!(parse(warned).is_ok());
    }
}

// --- Groups ----------------------------------------------------------------

#[cfg(test)]
mod group_tests {
    use super::*;

    fn sev_of(diags: &[Diagnostic], id: &str) -> Option<Severity> {
        diags
            .iter()
            .find(|d| d.id() == Some(id))
            .map(|d| d.severity)
    }

    const GROUPED: &str = r#"
[[groups]]
id = "work-slack"
name = "Work Slack"
type = "slack"

[[steps]]
id = "work-slack/ingest"
command = "fetch-slack"

[[steps]]
id = "work-slack/render_markdown"
command = "render-slack"
inputs = ["work-slack/ingest"]

[[groups]]
id = "unified_document_index"

[[steps]]
id = "unified_document_index/grid_index"
command = "index-grid"
inputs = ["work-slack/render_markdown"]

[[applets]]
group = "unified_document_index"
id = "unified_document_index"
command = "datalib-applet unified_document_index"
"#;

    /// Groups and the steps under them load clean, and each step's group
    /// is its id's first part.
    #[test]
    fn the_grouped_shape_loads_clean() {
        let check = check_text(GROUPED);
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let mut ids: Vec<&str> = check.cfg.steps.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "unified_document_index/grid_index",
                "work-slack/ingest",
                "work-slack/render_markdown"
            ]
        );
        assert_eq!(check.cfg.groups.len(), 2);
        assert_eq!(check.cfg.groups[0].name.as_deref(), Some("Work Slack"));
        assert_eq!(
            check.cfg.applets[0].group.as_deref(),
            Some("unified_document_index")
        );
        assert!(check.graph.by_id.contains_key("work-slack/render_markdown"));
        let render = &check.graph.steps[check.graph.by_id["work-slack/render_markdown"]];
        assert_eq!(render.group.as_deref(), Some("work-slack"));
    }

    /// A rejected group costs the group and every step under it — and
    /// those steps are told the fix is on the group, not on them.
    #[test]
    fn a_bad_group_takes_its_steps_with_it_as_blocked() {
        let check = check_text(
            "[[groups]]\nid = \"a\"\nsource = \"x\"\n\n\
             [[steps]]\nid = \"a/raw\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"fine/raw\"\ncommand = \"y\"\n",
        );
        assert_eq!(sev_of(&check.diagnostics, "a"), Some(Severity::Rejected));
        let step = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("a/raw"))
            .unwrap();
        assert_eq!(step.severity, Severity::Blocked);
        assert!(step.message.contains("was itself dropped"), "{step:?}");
        assert!(check.cfg.groups.is_empty());
        let ids: Vec<&str> = check.cfg.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["fine/raw"]);
    }

    #[test]
    fn a_group_id_is_one_segment_and_never_system() {
        for bad in ["", "a/b", "..", "-x", "a b", "system", "datalib_curated"] {
            let check = check_text(&format!("[[groups]]\nid = \"{bad}\"\n"));
            assert_eq!(check.cfg.groups.len(), 0, "{bad:?} should be rejected");
            assert_eq!(check.diagnostics[0].severity, Severity::Rejected);
            assert!(!usable_group_id(bad), "{bad:?}");
        }
        for good in ["a", "work-slack", "a.b_c", "unified_document_index"] {
            let check = check_text(&format!(
                "[[groups]]\nid = \"{good}\"\n[[steps]]\nid = \"{good}/raw\"\n\
                 command = \"x\"\n"
            ));
            assert!(check.is_clean(), "{good:?}: {:?}", check.diagnostics);
            assert!(usable_group_id(good), "{good:?}");
        }
    }

    /// Two groups with one id would both claim every step under it; the
    /// first keeps the id, as with steps, and the steps stay under it.
    #[test]
    fn the_first_of_two_duplicate_groups_wins() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\nname = \"first\"\n\n[[groups]]\nid = \"g\"\nname = \"second\"\n\n\
             [[steps]]\nid = \"g/raw\"\ncommand = \"x\"\n",
        );
        assert_eq!(check.cfg.groups.len(), 1);
        assert_eq!(check.cfg.groups[0].name.as_deref(), Some("first"));
        assert_eq!(check.cfg.steps.len(), 1, "the step still resolves");
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].severity, Severity::Rejected);
    }

    /// A declared group gives a step no namespace of its own: a step
    /// claiming the tree of one under a group collides with it as any
    /// other would.
    #[test]
    fn grouped_and_ungrouped_ids_collide_in_one_namespace() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\n\n\
             [[steps]]\nid = \"g/raw\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"g/raw\"\ncommand = \"y\"\n\n\
             [[steps]]\nid = \"g\"\ncommand = \"z\"\n",
        );
        assert_eq!(check.cfg.steps.len(), 1);
        let msgs: Vec<&str> = check
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(msgs.iter().any(|m| m.contains("duplicate id")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("nested with")), "{msgs:?}");
    }

    /// A group nothing is filed under warns, and drops nothing; a step's
    /// `name` says nothing at all.
    #[test]
    fn an_empty_group_warns_without_dropping() {
        let check = check_text(
            "[[groups]]\nid = \"lonely\"\n\n[[groups]]\nid = \"g\"\n\n\
             [[steps]]\nid = \"g/raw\"\nname = \"Raw\"\ncommand = \"x\"\n",
        );
        assert_eq!(check.dropped(), 0);
        assert!(check.nothing_dropped());
        assert!(!check.is_clean());
        assert_eq!(check.cfg.groups.len(), 2);
        assert_eq!(check.cfg.steps.len(), 1);
        let lonely = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("lonely"))
            .unwrap();
        assert_eq!(lonely.severity, Severity::Warning);
        assert!(lonely.message.contains("no steps"), "{lonely:?}");
        assert_eq!(check.diagnostics.len(), 1, "{:?}", check.diagnostics);
    }

    /// A group whose only step was rejected is not empty — somebody filled
    /// it, and the entry to fix is the step.
    #[test]
    fn a_group_whose_step_was_rejected_is_not_called_empty() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\n\n\
             [[steps]]\nid = \"g/raw\"\ncommand = \"x\"\ntitle = 1\n",
        );
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].id(), Some("g/raw"));
    }

    /// An applet's group is a filing, so a wrong one is advice rather than a
    /// reason to take the grid down.
    #[test]
    fn an_applet_under_an_undeclared_group_only_warns() {
        let check = check_text(
            "[[applets]]\ngroup = \"nope\"\nid = \"unified_document_index\"\ncommand = \"x\"\n",
        );
        assert_eq!(check.cfg.applets.len(), 1);
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].severity, Severity::Warning);
    }

    /// A custom step outside any group is still legal and still writes its
    /// id verbatim — the shape a shell script or a third-party program uses.
    #[test]
    fn an_ungrouped_custom_step_keeps_its_verbatim_id() {
        let check = check_text(
            "[[steps]]\nid = \"exports/csv\"\ncommand = \"my-exporter\"\nname = \"CSV export\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let spec = &check.graph.steps[0];
        assert_eq!(spec.id, "exports/csv");
        assert_eq!(spec.group, None);
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use crate::supervisor::locks::Hold;

    fn step<'a>(check: &'a ConfigCheck, id: &str) -> &'a StepSpec {
        &check.graph.steps[check.graph.by_id[id]]
    }

    fn slots(check: &ConfigCheck, name: &str) -> Option<usize> {
        check
            .graph
            .locks
            .iter()
            .find(|l| l.name == name)
            .map(|l| l.slots)
    }

    /// Both ways to write `locks`, and `reads`: a list holds each once
    /// and shared, a table says how.
    #[test]
    fn a_step_holds_the_locks_it_names_as_it_names_them() {
        let check = check_text(
            r#"
[[locks]]
name = "quota"

[[locks]]
name = "gpu"
slots = 2

[[steps]]
id = "a"
command = "x"
locks = ["quota", "quota"]

[[steps]]
id = "b"
command = "x"
locks = { gpu = "exclusive" }
reads = "files"
inputs = ["a"]
"#,
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert_eq!(slots(&check, "quota"), Some(1));
        assert_eq!(slots(&check, "gpu"), Some(2));
        assert_eq!(
            step(&check, "a").locks,
            Some(vec![("quota".to_string(), Hold::Shared)])
        );
        let b = step(&check, "b");
        assert_eq!(b.locks, Some(vec![("gpu".to_string(), Hold::Exclusive)]));
        assert!(!b.reads_pinned);
    }

    /// A step reads files off disk unless it says it reads pinned, so a
    /// step that says nothing never runs beside a writer of its inputs.
    #[test]
    fn reads_defaults_to_files() {
        let check = check_text(
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"b\"\ncommand = \"x\"\ninputs = [\"a\"]\n\n\
             [[steps]]\nid = \"c\"\ncommand = \"x\"\ninputs = [\"a\"]\nreads = \"pinned\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert!(!step(&check, "b").reads_pinned, "unsaid is files");
        assert!(step(&check, "c").reads_pinned);
    }

    /// A step that names no lock holds `network` when it reads nothing
    /// and `cpu` when it reads something, whatever its group or program.
    #[test]
    fn a_step_that_names_no_lock_holds_network_or_cpu_by_its_inputs() {
        let check = check_text(
            r#"
[[groups]]
id = "mail"
type = "email"

[[groups]]
id = "unified_document_index"

[[steps]]
id = "mail/ingest"
command = "fetch"

[[steps]]
id = "mail/render_markdown"
command = "render"
inputs = ["mail/ingest"]

[[steps]]
id = "unified_document_index/grid_index"
command = "index"
inputs = ["mail/render_markdown"]

[[steps]]
id = "loose"
command = "x"
inputs = ["mail/ingest"]
"#,
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let held = |id: &str| {
            crate::supervisor::locks::held_by(step(&check, id))
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(held("mail/ingest"), ["network"]);
        assert_eq!(held("mail/render_markdown"), ["cpu"]);
        assert_eq!(held("unified_document_index/grid_index"), ["cpu"]);
        assert_eq!(held("loose"), ["cpu"]);
    }

    /// The runner defines two locks; any other a step holds, the config
    /// declares — the qmd locks included.
    #[test]
    fn only_network_and_cpu_need_no_declaring() {
        for name in ["index", "qmd_keyword", "qmd_embed"] {
            let check = check_text(&format!(
                "[[steps]]\nid = \"a\"\ncommand = \"x\"\nlocks = [\"{name}\"]\n"
            ));
            assert!(check.graph.steps.is_empty(), "{name}");
            assert!(check.diagnostics[0].message.contains(name), "{name}");
        }
    }

    #[test]
    fn a_step_holding_a_lock_no_one_declared_is_rejected() {
        let check = check_text("[[steps]]\nid = \"a\"\ncommand = \"x\"\nlocks = [\"nope\"]\n");
        assert!(check.graph.steps.is_empty());
        let d = &check.diagnostics[0];
        assert_eq!(d.severity, Severity::Rejected);
        assert!(d.message.contains("nope"), "{}", d.message);
    }

    #[test]
    fn start_after_inputs_settle_loads_and_a_misspelling_is_rejected() {
        let check = check_text(
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"b\"\ncommand = \"x\"\ninputs = [\"a\"]\nstart = \"after_inputs_settle\"\n\n\
             [[steps]]\nid = \"c\"\ncommand = \"x\"\ninputs = [\"a\"]\nstart = \"on_each_seal\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert!(step(&check, "b").waits_for_settled_inputs);
        assert!(!step(&check, "c").waits_for_settled_inputs);
        assert!(!step(&check, "a").waits_for_settled_inputs);

        let check = check_text("[[steps]]\nid = \"a\"\ncommand = \"x\"\nstart = \"later\"\n");
        assert!(check.graph.steps.is_empty());
        let d = &check.diagnostics[0];
        assert_eq!(d.severity, Severity::Rejected);
        assert!(d.message.contains("later"), "{}", d.message);
    }

    #[test]
    fn a_lock_declared_twice_or_with_no_slots_is_rejected() {
        let check = check_text(
            "[[locks]]\nname = \"q\"\n\n[[locks]]\nname = \"q\"\n\n[[locks]]\nname = \"z\"\nslots = 0\n",
        );
        let rejected: Vec<&str> = check
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Rejected)
            .filter_map(|d| d.id())
            .collect();
        assert_eq!(rejected, ["q", "z"], "{:?}", check.diagnostics);
        assert_eq!(slots(&check, "q"), Some(1), "the first stands");
        assert_eq!(slots(&check, "z"), None);
    }

    /// The budgets are locks every config has; declaring one resizes it.
    #[test]
    fn declaring_a_default_lock_resizes_it() {
        let check = check_text("[[locks]]\nname = \"network\"\nslots = 1\n");
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let names: Vec<&str> = check.graph.locks.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["network", "cpu"]);
        assert_eq!(slots(&check, "network"), Some(1));
        assert_eq!(slots(&check, "cpu"), Some(4));
    }

    /// When a step may run is not what it makes: neither re-runs anything.
    #[test]
    fn locks_reads_and_start_are_not_in_the_fingerprint() {
        let plain = check_text("[[steps]]\nid = \"a\"\ncommand = \"x\"\n");
        let held = check_text(
            "[[locks]]\nname = \"q\"\n\n[[steps]]\nid = \"a\"\ncommand = \"x\"\nlocks = [\"q\"]\nreads = \"files\"\nstart = \"after_inputs_settle\"\n",
        );
        assert_eq!(plain.graph.fingerprints, held.graph.fingerprints);
    }

    #[test]
    fn start_when_as_str_matches_the_serde_spelling() {
        use strum::VariantArray;
        for &v in StartWhen::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(StartWhen::parse(v.as_str()), Some(v));
        }
    }

    /// strum and serde spell these independently: the config is read with
    /// serde, and anything that names a mode back uses strum.
    #[test]
    fn reads_as_str_matches_the_serde_spelling() {
        use strum::VariantArray;
        for &v in Reads::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(Reads::parse(v.as_str()), Some(v));
        }
    }

    const PACKAGE_SHA: &str = "3f9a1c2e3f9a1c2e3f9a1c2e3f9a1c2e3f9a1c2e3f9a1c2e3f9a1c2e3f9a1c2e";

    fn with_package(step: &str) -> String {
        format!(
            "[[packages]]\nid = \"weather\"\nurl = \"https://h/datalib-package.toml\"\n\
             sha256 = \"{PACKAGE_SHA}\"\n\n[[groups]]\nid = \"sfo\"\ntype = \"weather\"\n\n\
             [[steps]]\nid = \"sfo/ingest\"\n{step}\n"
        )
    }

    const PACKAGE_DIR: &str = "system/packages/3f9a1c2e3f9a1c2e";

    fn argv_of(check: &ConfigCheck, id: &str) -> Vec<String> {
        let spec = check.graph.steps.iter().find(|s| s.id == id).unwrap();
        let StepRun::Subprocess { argv, .. } = &spec.run else {
            panic!("{:?}", spec.run)
        };
        argv.clone()
    }

    /// A package's program is named by its path under the data root, like
    /// any other relative path: the pin is in that path, so an update
    /// changes argv and the step re-runs.
    #[test]
    fn a_package_step_runs_the_path_its_command_names() {
        let check = check_text(&with_package(&format!(
            "command = \"{PACKAGE_DIR}/bin/fetch-weather --station KSFO\""
        )));
        assert!(check.nothing_dropped(), "{:?}", check.diagnostics);
        assert_eq!(
            argv_of(&check, "sfo/ingest")[..3],
            [
                format!("{PACKAGE_DIR}/bin/fetch-weather"),
                "--station".to_string(),
                "KSFO".to_string()
            ]
        );
    }

    #[test]
    fn a_command_in_a_package_no_entry_pins_is_dropped() {
        let other = "system/packages/0000000000000000/bin/fetch-weather";
        for text in [
            with_package(&format!("command = \"{other}\"")),
            format!(
                "{}\n[[applets]]\nid = \"chart\"\ncommand = \"{other}\"\n",
                with_package("command = \"fetch\"")
            ),
        ] {
            let check = check_text(&text);
            assert_eq!(check.dropped(), 1, "{text}");
            assert!(
                check
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("no `[[packages]]` entry pins")),
                "{:?}",
                check.diagnostics
            );
        }
    }

    #[test]
    fn a_package_pin_must_be_a_sha256() {
        let text = with_package("command = \"fetch\"").replace(PACKAGE_SHA, "abc");
        let check = check_text(&text);
        assert!(check.cfg.packages.is_empty());
        assert!(check
            .diagnostics
            .iter()
            .any(|d| d.message.contains("not a sha256")));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::step::Verb;

    fn spec<'a>(check: &'a ConfigCheck, id: &str) -> &'a StepSpec {
        &check.graph.steps[check.graph.by_id[id]]
    }

    fn rejection<'a>(check: &'a ConfigCheck, needle: &str) -> &'a Diagnostic {
        check
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Rejected && d.describe().contains(needle))
            .unwrap_or_else(|| panic!("no rejection naming {needle:?}: {:?}", check.diagnostics))
    }

    /// A step's group is its id's first part when a `[[groups]]` entry
    /// has that id, and none otherwise; nothing about the step says it.
    #[test]
    fn a_steps_group_is_its_ids_first_part_when_declared() {
        let check = check_text(
            "[[groups]]\nid = \"mail\"\n\n\
             [[steps]]\nid = \"mail/ingest\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"mail/deep/out\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"loose/out\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"mail_like\"\ncommand = \"x\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert_eq!(spec(&check, "mail/ingest").group.as_deref(), Some("mail"));
        assert_eq!(spec(&check, "mail/deep/out").group.as_deref(), Some("mail"));
        assert_eq!(spec(&check, "loose/out").group, None);
        assert_eq!(spec(&check, "mail_like").group, None);
    }

    /// Each way a config can still be in the shape from before the runner
    /// stopped knowing `datalib-step` is refused, and each refusal names
    /// the rewrite.
    #[test]
    fn each_retired_shape_is_refused_and_names_the_rewrite() {
        let step_cases = [
            (
                "[[groups]]\nid = \"g\"\n\n[[steps]]\ngroup = \"g\"\nfunction = \"raw\"\ncommand = \"x\"\n",
                "`group` and `function`",
            ),
            (
                "[[steps]]\nid = \"g/raw\"\nfunction = \"raw\"\ncommand = \"x\"\n",
                "`group` and `function`",
            ),
            ("[[steps]]\nid = \"g/raw\"\n", "missing field `command`"),
        ];
        for (text, said) in step_cases {
            let check = check_text(text);
            assert!(check.graph.steps.is_empty(), "{text}");
            let d = rejection(&check, said);
            assert!(
                d.describe().contains(RETIRED_SHAPE_HELP),
                "{}",
                d.describe()
            );
        }

        for (text, key) in [
            (
                "[checkpoint_cadence]\nat_most_every_secs = 2.5\n",
                "checkpoint_cadence",
            ),
            ("[render_pages]\nbuckets = 200\n", "render_pages"),
            ("embed_contexts = 4\n", "embed_contexts"),
        ] {
            let check = check_text(&format!("{text}\n[[steps]]\nid = \"a\"\ncommand = \"x\"\n"));
            assert!(!check.is_fatal(), "{key}: {:?}", check.diagnostics);
            let d = rejection(&check, key);
            assert!(
                d.describe().contains(RETIRED_SHAPE_HELP),
                "{}",
                d.describe()
            );
            assert_eq!(d.line, Some(1), "{key}: points at the key");
            assert_eq!(check.graph.steps.len(), 1, "{key}: the steps still load");
        }

        let check = check_text("[[groups]]\nid = \"d\"\ntype = \"diff\"\nsource = \"slack\"\n");
        assert!(check.cfg.groups.is_empty());
        let d = rejection(&check, "`source`");
        assert!(
            d.describe().contains(RETIRED_SHAPE_HELP),
            "{}",
            d.describe()
        );
    }

    /// A step is sent a verb only if its program's `[[programs]]` entry
    /// lists it, the program being its command's first word as written.
    #[test]
    fn a_steps_verbs_are_its_programs_entry() {
        let check = check_text(
            "[[programs]]\ncommand = \"fetch\"\nverbs = [\"reset\", \"prepare\"]\n\n\
             [[programs]]\ncommand = \"bin/shrink\"\nverbs = [\"compact\", \"collect\"]\n\n\
             [[steps]]\nid = \"a\"\ncommand = \"fetch --since 2026\"\n\n\
             [[steps]]\nid = \"b\"\ncommand = \"./fetch\"\n\n\
             [[steps]]\nid = \"c\"\ncommand = \"bin/shrink\"\n\n\
             [[steps]]\nid = \"d\"\ncommand = \"other fetch\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let verbs = |id: &str| spec(&check, id).verbs.iter().copied().collect::<Vec<_>>();
        assert_eq!(verbs("a"), [Verb::Reset, Verb::Prepare]);
        assert_eq!(verbs("b"), [], "compared as written");
        assert_eq!(verbs("c"), [Verb::Compact, Verb::Collect]);
        assert_eq!(verbs("d"), [], "only the first word is the program");
    }

    #[test]
    fn a_programs_entry_names_one_program_once() {
        let check = check_text(
            "[[programs]]\ncommand = \"fetch\"\nverbs = [\"reset\"]\n\n\
             [[programs]]\ncommand = \"fetch\"\nverbs = [\"prepare\"]\n\n\
             [[programs]]\ncommand = \"fetch --x\"\nverbs = [\"prepare\"]\n\n\
             [[steps]]\nid = \"a\"\ncommand = \"fetch\"\n",
        );
        rejection(&check, "declared twice");
        rejection(&check, "not one program");
        assert_eq!(
            spec(&check, "a").verbs.iter().copied().collect::<Vec<_>>(),
            [Verb::Reset],
            "the first entry stands"
        );
    }

    /// A misspelled verb costs that verb, with a warning, and the file and
    /// the program's other verbs load.
    #[test]
    fn an_unknown_verb_is_a_warning_and_the_rest_load() {
        let check = check_text(
            "[[programs]]\ncommand = \"fetch\"\nverbs = [\"reset\", \"prepair\"]\n\n\
             [[steps]]\nid = \"a\"\ncommand = \"fetch\"\n",
        );
        assert!(!check.is_fatal());
        assert!(
            check
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Warning && d.message.contains("\"prepair\"")),
            "{:?}",
            check.diagnostics
        );
        assert_eq!(
            spec(&check, "a").verbs.iter().copied().collect::<Vec<_>>(),
            [Verb::Reset]
        );
    }

    /// strum and serde spell a verb independently: the config is read
    /// with strum, and the step's spec serializes it with serde.
    #[test]
    fn verb_as_str_matches_the_serde_spelling() {
        use strum::VariantArray;
        for &v in Verb::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(Verb::parse(v.as_str()), Some(v));
            assert_eq!(v.flag(), format!("--{}", v.as_str()));
        }
    }

    /// Params travel in one environment variable, which Linux caps; a
    /// step whose params would not fit is refused at load, not at spawn.
    #[test]
    fn params_over_the_limit_are_refused() {
        let with = |n: usize| {
            check_text(&format!(
                "[[steps]]\nid = \"a\"\ncommand = \"x\"\nparams.blob = \"{}\"\n",
                "x".repeat(n)
            ))
        };
        let fits = with(MAX_PARAMS_BYTES - 64);
        assert!(fits.is_clean(), "{:?}", fits.diagnostics);
        let over = with(MAX_PARAMS_BYTES);
        assert!(over.graph.steps.is_empty());
        rejection(&over, "at most 64 KiB");
    }

    /// `[env]` is kept for the host to hand on; a key that cannot be a
    /// variable is refused and left out.
    #[test]
    fn env_keys_must_be_variable_names() {
        let check = check_text("[env]\nGOOD_1 = \"a\"\n\"1BAD\" = \"b\"\n\"A-B\" = \"c\"\n");
        assert_eq!(
            check.cfg.env,
            BTreeMap::from([("GOOD_1".to_string(), "a".to_string())])
        );
        rejection(&check, "\"1BAD\"");
        rejection(&check, "\"A-B\"");
    }
}
