//! The config file, `config.toml`: `[[groups]]` the entries are filed
//! under, `[[steps]]` the runner schedules, and `[[applets]]` the http
//! gateway spawns on demand. This crate reads the file and checks each
//! entry; every program that reads or writes a config links it. Turning
//! the steps into the graph the runner runs is the runner's
//! (`datalib_dag::config_check`).
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

pub mod array;
pub mod diagnostics;
pub mod disk_floor;
pub mod input_pattern;
pub mod lex;
pub mod locks;
pub mod order;
pub mod written;

pub use diagnostics::{Diagnostic, EntryKind, EntryRef, Severity};

use crate::disk_floor::DiskFloor;
use crate::input_pattern::Expansion;
use crate::locks::Hold;

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
    /// with a `type`; the unified grid and search index is a group without one.
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
    /// ([`locks::DEFAULT_LOCKS`]).
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
    Held(BTreeMap<String, Hold>),
}

impl StepLocks {
    pub fn held(&self) -> Vec<(String, Hold)> {
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

/// A verb the runner may append to a step's argv, as `--<verb>`, when the
/// step's program declares it (`docs/dev/step_protocol.md` § Verbs).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Verb {
    /// Empty what the step wrote. A program that does not take it has its
    /// step's directory deleted by the runner instead.
    Reset,
    /// Each time the runner starts: bring what the step wrote to this
    /// program's shape, or say it cannot.
    Prepare,
    /// Give back disk, dropping history nothing reads.
    Compact,
    /// Give back disk, keeping every version.
    Collect,
    /// Before anything runs: say whether the step would run its params as
    /// written, and write nothing.
    Check,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// The argv word the step is sent.
    pub fn flag(self) -> String {
        format!("--{}", self.as_str())
    }
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
    /// applet's `DATALIB_DAG_PARAMS`.
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
    /// its shape gives it (`datalib_dag::supervisor::locks::default_for`).
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

    /// Its command split shell-style into the argv the runner starts.
    pub fn argv(&self) -> Result<Vec<String>> {
        let argv = shlex::split(&self.command)
            .with_context(|| format!("command {:?} has unbalanced quoting", self.command))?;
        if argv.is_empty() {
            bail!("empty command");
        }
        Ok(argv)
    }

    /// Its `params` as the JSON text the step reads from
    /// `DATALIB_DAG_PARAMS`, at most [`MAX_PARAMS_BYTES`] long.
    pub fn params_text(&self) -> Result<Option<String>> {
        let Some(params) = &self.params else {
            return Ok(None);
        };
        let json =
            serde_json::to_string(&params_to_json(params, &self.id)?).context("params → JSON")?;
        if json.len() > MAX_PARAMS_BYTES {
            bail!(
                "params are {} bytes as JSON; a step's params may be at most {} KiB",
                json.len(),
                MAX_PARAMS_BYTES / 1024
            );
        }
        Ok(Some(json))
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
/// (`datalib_dag::config_check::check_text`'s job).
pub fn is_toml(text: &str) -> bool {
    !parse_graded(text)
        .1
        .iter()
        .any(|d| d.severity == Severity::Fatal)
}

/// Load + resolve a config file, strictly: one error instead of every
/// problem.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg = parse(&text).with_context(|| format!("parse {}", path.display()))?;
    let root = data_root_of(path, &cfg);
    Ok((cfg, root))
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

/// The strict view of [`accept_entries`]. Note what this does *not* do: it
/// resolves nothing between steps, so an `inputs` entry naming no declared
/// step passes here and is caught when the runner builds its graph.
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

/// A step's or an applet's `params` subtree as the JSON the child reads
/// from `DATALIB_DAG_PARAMS`.
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
    #[serde(default)]
    embed_search_index: Option<toml::Spanned<toml::Value>>,
    #[serde(default)]
    modal: Option<toml::Spanned<toml::Value>>,
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
    groups: Vec<GroupEntry>,
    /// Each step with its patterns expanded over every step this pass
    /// kept.
    steps: Vec<StepEntry>,
    /// The same steps with their `inputs` as written, for the runner to
    /// expand again over what its graph keeps.
    written: Vec<StepEntry>,
    expansions: Vec<Expansion>,
    applets: Vec<AppletEntry>,
    diagnostics: Vec<Diagnostic>,
}

/// `steps` with every pattern in their `inputs` expanded to the ids in
/// `universe` it names, and what each pattern stood for.
pub fn expand_inputs(steps: &[StepEntry], universe: &[&str]) -> (Vec<StepEntry>, Vec<Expansion>) {
    let mut out = Vec::with_capacity(steps.len());
    let mut expansions = Vec::new();
    for written in steps {
        let (inputs, found) = input_pattern::expand(&written.id, &written.inputs, universe);
        expansions.extend(found);
        out.push(StepEntry {
            inputs,
            ..written.clone()
        });
    }
    (out, expansions)
}

/// Every rule decidable without the graph, applied entry by entry, dropping
/// what fails and saying why. Groups first, because a step's rules depend on
/// which groups exist; steps and applets after, each against that list.
fn accept_entries(c: Candidates, text: Option<&str>) -> Accepted {
    let mut diags = Vec::new();
    let (locks, lock_names, lock_diags) = accept_locks(c.locks, text);
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

    let (_, program_diags) = accept_programs(&c.programs);
    diags.extend(program_diags);
    let by_id: BTreeMap<&str, &GroupEntry> = groups.iter().map(|g| (g.id.as_str(), g)).collect();
    let (written, step_diags) = accept_steps(
        c.steps,
        &by_id,
        &dropped_groups,
        &lock_names,
        &package_dirs,
        text,
    );
    diags.extend(step_diags);
    let universe: Vec<&str> = written.iter().map(|w| w.id.as_str()).collect();
    let (steps, expansions) = expand_inputs(&written, &universe);

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
        groups,
        steps,
        written,
        expansions,
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
/// Returns the name of every lock a step may hold: the defaults, then the
/// ones the config adds.
fn accept_locks(
    candidates: Vec<Candidate<LockEntry>>,
    text: Option<&str>,
) -> (Vec<LockEntry>, Vec<String>, Vec<Diagnostic>) {
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
    let mut names: Vec<String> = locks::DEFAULT_LOCKS.map(str::to_string).to_vec();
    for l in &accepted {
        if !names.contains(&l.name) {
            names.push(l.name.clone());
        }
    }
    (accepted, names, diags)
}

/// The `[[programs]]` rules: a program is named once, and a verb once per
/// program. Returns each program's verbs.
fn accept_programs(
    programs: &[ProgramEntry],
) -> (BTreeMap<String, BTreeSet<Verb>>, Vec<Diagnostic>) {
    let mut out: BTreeMap<String, BTreeSet<Verb>> = BTreeMap::new();
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
            match Verb::parse(word) {
                Some(verb) => {
                    verbs.insert(verb);
                }
                None => diags.push(Diagnostic::new(
                    Severity::Warning,
                    format!(
                        "[[programs]] {command:?}: {word:?} is not a verb this runner sends \
                         (reset, prepare, compact, collect, check); it is ignored"
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
/// Later entries lose to earlier ones. Whether an input names a step is
/// checked when the runner builds its graph, the first place the full id
/// set exists.
fn accept_steps(
    candidates: Vec<Candidate<StepEntry>>,
    groups: &BTreeMap<&str, &GroupEntry>,
    dropped_groups: &BTreeSet<String>,
    locks: &[String],
    packages: &BTreeMap<&str, String>,
    text: Option<&str>,
) -> (Vec<StepEntry>, Vec<Diagnostic>) {
    let mut accepted: Vec<StepEntry> = Vec::with_capacity(candidates.len());
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
        if let Err(e) = runnable(&c.entry) {
            diags.push(c.diag(Severity::Rejected, text, Some("command"), format!("{e:#}")));
            continue;
        }
        // A step that names no lock holds a default one, which every
        // config has, so only the locks it names can be undeclared.
        let undeclared: Vec<String> = c
            .entry
            .locks
            .as_ref()
            .map(StepLocks::held)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !locks.contains(name))
            .collect();
        if !undeclared.is_empty() {
            let declared: Vec<&str> = locks.iter().map(String::as_str).collect();
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
        accepted.push(c.entry);
    }
    (accepted, diags)
}

/// Whether the runner can start the step as written: its command splits
/// into an argv, its params become JSON small enough to hand over, and
/// every input is a path under the data root.
fn runnable(e: &StepEntry) -> Result<()> {
    e.argv()?;
    e.params_text()?;
    for i in &e.inputs {
        artifact_path(i)?;
    }
    Ok(())
}

/// A path relative to the data root, with the surrounding `/`s trimmed:
/// what a step's id and each of its inputs must be.
pub fn artifact_path(raw: &str) -> Result<&str> {
    let raw = raw.trim_matches('/');
    if raw.is_empty() {
        bail!("artifact path must be a non-empty relative path");
    }
    for s in raw.split('/') {
        match s {
            "" => bail!("artifact path {raw:?} has an empty segment"),
            "." | ".." => bail!("artifact path {raw:?} may not contain `.`/`..`"),
            _ => {}
        }
    }
    Ok(raw)
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
pub struct Entries {
    /// The entries that passed, each step with its patterns expanded over
    /// the steps this pass kept.
    pub cfg: DagConfig,
    /// step id → byte range of the `[[steps]]` header. Includes dropped
    /// steps: a graph diagnostic naming one still wants somewhere to point.
    pub spans: BTreeMap<String, std::ops::Range<usize>>,
    /// The step ids this pass threw out. Handed to graph assembly so a step
    /// whose input names one of them is told its input was *dropped* rather
    /// than that it never existed — different sentences, different entries
    /// to fix.
    pub dropped: BTreeSet<String>,
    /// The kept steps with their `inputs` as written, in `cfg.steps`' order.
    pub written: Vec<StepEntry>,
    pub expansions: Vec<Expansion>,
    pub diagnostics: Vec<Diagnostic>,
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
pub fn entries_of(text: &str) -> Entries {
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
                spans: BTreeMap::new(),
                dropped: BTreeSet::new(),
                written: Vec::new(),
                expansions: Vec::new(),
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
        ("embed_search_index", &raw.embed_search_index),
        ("modal", &raw.modal),
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

    let steps = accepted.steps;

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
        spans,
        dropped,
        written: accepted.written,
        expansions: accepted.expansions,
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
/// unresolved — `datalib_dag::config_check::check_text` is the whole
/// answer.
pub fn parse_graded(text: &str) -> (DagConfig, Vec<Diagnostic>) {
    let e = entries_of(text);
    (e.cfg, e.diagnostics)
}

impl DagConfig {
    /// Each `[[programs]]` entry's verbs, by program, leaving out the
    /// entries and verbs the entry rules warn about or refuse.
    pub fn verbs_by_program(&self) -> BTreeMap<String, BTreeSet<Verb>> {
        accept_programs(&self.programs).0
    }

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
mod tests {
    use super::*;

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
