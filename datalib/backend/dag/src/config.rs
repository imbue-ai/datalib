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
    /// with a `type`; the unified index is a group without one.
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
    /// How often a step seals what it has written, so a consumer can see it
    /// before the step finishes. Omitted means the step's own default.
    #[serde(default)]
    pub checkpoint_cadence: Option<CheckpointCadence>,
}

/// The latency/history tradeoff, in seconds, as a person writes it in
/// `config.toml`.
///
/// Plain numbers rather than the `etl` type they become: the runner does not
/// link `etl`, and does not need to — it forwards this to the step, which
/// owns what a checkpoint *is*.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCadence {
    /// Seal once writes have been quiet this long.
    pub quiet_for_secs: f64,
    /// Seal anyway once this long has passed since the last commit, however
    /// busy the writer is.
    pub at_most_every_secs: f64,
}

impl CheckpointCadence {
    /// The wire form: `"<quiet>,<ceiling>"`, seconds. One env var rather
    /// than two, so a step reads the pair or neither.
    pub fn encode(&self) -> String {
        format!("{},{}", self.quiet_for_secs, self.at_most_every_secs)
    }

    /// `None` for anything this build cannot read — a malformed value from a
    /// newer config is not a reason to fail the run, and the step's default
    /// cadence is a safe answer.
    pub fn decode(s: &str) -> Option<Self> {
        let (q, c) = s.split_once(',')?;
        let quiet_for_secs: f64 = q.trim().parse().ok()?;
        let at_most_every_secs: f64 = c.trim().parse().ok()?;
        if !(quiet_for_secs.is_finite() && at_most_every_secs.is_finite())
            || quiet_for_secs < 0.0
            || at_most_every_secs < 0.0
        {
            return None;
        }
        Some(Self {
            quiet_for_secs,
            at_most_every_secs,
        })
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
    /// The kind of data this group mirrors (`slack`, `email`, …), which
    /// is what makes it a *source*. Forwarded to every step under the group
    /// and folded into their fingerprints. Absent for a group that mirrors
    /// nothing, such as the unified index.
    #[serde(default)]
    pub r#type: Option<String>,
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
    /// The command to run, split shell-style, resolved the same way a step's
    /// is (`binary_dir`, then `PATH`).
    pub command: String,
    /// Arbitrary applet parameters, forwarded verbatim as JSON via `--params`.
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

/// One `[[steps]]` entry with its id settled. [`StepTable`] is the entry as
/// written; this is what the rest of the crate sees, so nothing downstream
/// has to know how an id came to be.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "StepTable")]
pub struct StepEntry {
    /// The tree this step writes and the key its state is recorded under.
    /// `<group>/<function>` for a grouped step, the written `id` otherwise.
    pub id: String,
    /// The `[[groups]]` entry this step belongs to. `None` for a custom step
    /// that wrote a verbatim `id`.
    pub group: Option<String>,
    /// What this step does within its group. Always present with `group`
    /// and never without it.
    pub function: Option<String>,
    /// Free text the runner never reads. On a grouped step the label comes
    /// from the group and the function, so a `name` here is reported as
    /// probably unintended; on an ungrouped step it is the only label.
    pub name: Option<String>,
    /// The ids of the steps this one reads. A step id *is* the tree that step
    /// writes, so an entry here is both a step reference and an artifact path.
    /// A directory staged by hand is the `path` of a method table in
    /// `params` instead, and is not an artifact the DAG knows about.
    pub inputs: Vec<String>,
    /// The command to run, split shell-style into an argv. `None` means the
    /// built-in `datalib-step`, which reads its function and its group's
    /// type from the environment — so a step with no command needs a group.
    /// The child's cwd is `data_root`, so a relative multi-component argv[0]
    /// resolves against the data root; use a bare name or an absolute path
    /// for binaries elsewhere.
    pub command: Option<String>,
    /// Arbitrary step parameters, forwarded verbatim as JSON via
    /// `--params`.
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    pub env: BTreeMap<String, String>,
    /// Version of the step's own behavior, for steps whose output can change
    /// without their command line changing. Bumping it re-runs the step once,
    /// even though none of its inputs moved.
    pub code_version: Option<String>,
}

/// A `[[steps]]` table exactly as a person writes it. Either `group` and
/// `function` or `id`; the split is settled in `TryFrom` below so a
/// mis-written entry is refused with the key it is about.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepTable {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    function: Option<String>,
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
}

impl TryFrom<StepTable> for StepEntry {
    type Error = String;

    fn try_from(t: StepTable) -> std::result::Result<Self, String> {
        let id = match (&t.id, &t.group, &t.function) {
            (None, Some(g), Some(f)) => format!("{g}/{f}"),
            (Some(id), None, None) => id.clone(),
            (Some(_), Some(_), _) | (Some(_), None, Some(_)) => {
                return Err(
                    "`id` cannot be written on a step that has `group` or `function`: the \
                     id of a grouped step is composed as `<group>/<function>`, so remove \
                     `id` (or, for a custom step outside any group, remove the other two)"
                        .to_string(),
                )
            }
            (None, Some(_), None) => {
                return Err(
                    "missing field `function`: a step under a group says what it does \
                     there, and that word names the directory it writes"
                        .to_string(),
                )
            }
            (None, None, Some(_)) => {
                return Err(
                    "missing field `group`: `function` only means something within a group"
                        .to_string(),
                )
            }
            (None, None, None) => {
                return Err(
                    "missing field `group`: a step is `group` + `function`, or a custom \
                     step with a verbatim `id`"
                        .to_string(),
                )
            }
        };
        Ok(StepEntry {
            id,
            group: t.group,
            function: t.function,
            name: t.name,
            inputs: t.inputs,
            command: t.command,
            params: t.params,
            env: t.env,
            code_version: t.code_version,
        })
    }
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

fn data_root_of(path: &Path, cfg: &DagConfig) -> PathBuf {
    match &cfg.data_root {
        Some(p) => expand_tilde(p),
        None => {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            abs.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        }
    }
}

/// The one reserved top-level directory: the runner's and the server's own
/// state. A step writing there would put the scheduler's own bookkeeping
/// under its change detection. This is the policy; the path constants live in
/// `datalib_core::layout`, which this crate deliberately doesn't depend on.
pub const SYSTEM_DIR: &str = "system";

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

const SEGMENT_RULE: &str = "letters, digits, `.`, `_`, `-`, not starting with `-`, and \
                            never `.` or `..`";

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

/// A step's `params` subtree as the JSON the child gets on `--params`.
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
    let (_, diags) = accept_applets(
        candidates(&cfg.applets, EntryRef::applet, |a| Some(a.id.clone())),
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
    checkpoint_cadence: Option<CheckpointCadence>,
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
    groups: Vec<Candidate<GroupEntry>>,
    steps: Vec<Candidate<StepEntry>>,
    applets: Vec<Candidate<AppletEntry>>,
    /// Every `group = …` written on a step or an applet, counted before
    /// anything is dropped: a group whose only step was rejected is still a
    /// group somebody meant to fill, and calling it empty would send them to
    /// the wrong entry.
    named: BTreeSet<String>,
}

/// The strict door's candidates, from an already-deserialized config.
fn cfg_candidates(cfg: &DagConfig) -> Candidates {
    let mut named = BTreeSet::new();
    named.extend(cfg.steps.iter().filter_map(|s| s.group.clone()));
    named.extend(cfg.applets.iter().filter_map(|a| a.group.clone()));
    Candidates {
        groups: candidates(&cfg.groups, EntryRef::group, |g| Some(g.id.clone())),
        steps: candidates(&cfg.steps, EntryRef::step, |e| Some(e.id.clone())),
        applets: candidates(&cfg.applets, EntryRef::applet, |a| Some(a.id.clone())),
        named,
    }
}

/// What survived the entry rules, and what each problem cost.
struct Accepted {
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
    let (groups, group_diags) = accept_groups(c.groups, &c.named, text);
    let dropped_groups: BTreeSet<String> = group_diags
        .iter()
        .filter(|d| d.severity.drops_the_entry())
        .filter_map(|d| d.id().map(str::to_string))
        .collect();
    diags.extend(group_diags);

    let by_id: BTreeMap<&str, &GroupEntry> = groups.iter().map(|g| (g.id.as_str(), g)).collect();
    let (steps, step_diags) = accept_steps(c.steps, &by_id, &dropped_groups, text);
    diags.extend(step_diags);

    let (applets, applet_diags) = accept_applets(c.applets, text);
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
        groups,
        steps,
        applets,
        diagnostics: diags,
    }
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
        if !valid_id_segment(&id) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "group id {id:?} is not a usable directory name. Every step under a \
                     group writes into `<group id>/`, so the id must be one portable \
                     filename — {SEGMENT_RULE} — with no `/` in it."
                ),
            ));
            continue;
        }
        if id == SYSTEM_DIR {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "group id {SYSTEM_DIR:?} is reserved for the runner's and the server's \
                     own state; every step under it would write there."
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

/// The step rules, applied entry by entry. A grouped step's `function` must
/// be one directory name and its group must exist; a custom step's `id` must
/// be a usable path. Every step's id is unique, un-nested with every other,
/// and outside `system/`; the command must be runnable and `params` must be
/// JSON-able.
///
/// The nesting rule is the load-bearing one: two steps under one tree is two
/// writers on one `.doltlite_db`, whose working set is shared across
/// processes, so they commit each other's in-flight rows.
///
/// Later entries lose to earlier ones. Inputs are checked in
/// [`crate::Graph::build_graded`], the first place the full id set exists.
fn accept_steps(
    candidates: Vec<Candidate<StepEntry>>,
    groups: &BTreeMap<&str, &GroupEntry>,
    dropped_groups: &BTreeSet<String>,
    text: Option<&str>,
) -> (Vec<(StepEntry, StepSpec)>, Vec<Diagnostic>) {
    let mut accepted: Vec<(StepEntry, StepSpec)> = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for c in candidates {
        let id = c.entry.id.clone();
        let mut group_type: Option<&str> = None;
        match (&c.entry.group, &c.entry.function) {
            (Some(g), Some(f)) => {
                if !valid_id_segment(f) {
                    diags.push(c.diag(
                        Severity::Rejected,
                        text,
                        Some("function"),
                        format!(
                            "function {f:?} is not a usable directory name. A step writes \
                             `<group>/<function>/`, so the function must be one portable \
                             filename — {SEGMENT_RULE} — with no `/` in it."
                        ),
                    ));
                    continue;
                }
                match groups.get(g.as_str()) {
                    Some(group) => group_type = group.r#type.as_deref(),
                    None if dropped_groups.contains(g) => {
                        diags.push(
                            c.diag(
                                Severity::Blocked,
                                text,
                                Some("group"),
                                format!(
                                    "cannot run: its group {g:?} was itself dropped from this \
                                     config"
                                ),
                            )
                            .with_help(format!(
                                "nothing is wrong with this step — fix group {g:?} and this one \
                                 runs again"
                            )),
                        );
                        continue;
                    }
                    None => {
                        diags.push(
                            c.diag(
                                Severity::Rejected,
                                text,
                                Some("group"),
                                format!("group {g:?} names no declared group"),
                            )
                            .with_help(format!(
                                "declare it with a `[[groups]]` entry whose `id` is {g:?}. \
                                 Declared groups: {}",
                                id_list(groups.keys().copied())
                            )),
                        );
                        continue;
                    }
                }
            }
            _ => {
                if id.is_empty() || !id.split('/').all(valid_id_segment) {
                    diags.push(c.diag(
                        Severity::Rejected,
                        text,
                        Some("id"),
                        format!(
                            "id {id:?} is not a usable directory name. An id is the directory \
                             the step writes, so every `/`-separated segment must be a portable \
                             filename — {SEGMENT_RULE}."
                        ),
                    ));
                    continue;
                }
                if c.entry.command.is_none() {
                    diags.push(
                        c.diag(
                            Severity::Rejected,
                            text,
                            Some("id"),
                            "a step with no `command` runs `datalib-step`, which takes the \
                             provider from its group's `type` and the tree it writes from \
                             `<group>/<function>` — so it has to be declared under a group",
                        )
                        .with_help(
                            "write `group` and `function` instead of `id`, or give this step a \
                             `command` of its own",
                        ),
                    );
                    continue;
                }
            }
        }
        if let Some(word) = c.entry.command.as_deref().and_then(retired_subcommand) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    Some("command"),
                    format!(
                        "`datalib-step {word} …` is the shape written before `datalib-step` read \
                         its function from the environment, and it no longer runs. A built-in \
                         step is `group` + `function` with no `command` at all."
                    ),
                )
                .with_help("rewrite the file once: `datalib-migrate-config <data root> --force`"),
            );
            continue;
        }
        // Where the composed-vs-written distinction stops mattering: every
        // rule below is about the id itself. A grouped step's id has no key
        // of its own to point at, so its diagnostics land on the header.
        let id_key = if c.entry.group.is_some() {
            None
        } else {
            Some("id")
        };
        if id == SYSTEM_DIR || id.starts_with(&format!("{SYSTEM_DIR}/")) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                id_key,
                format!(
                    "id {id:?} writes under {SYSTEM_DIR:?}, which is reserved for the \
                     runner's and the server's own state."
                ),
            ));
            continue;
        }
        if seen.contains(&id) {
            diags.push(
                c.diag(
                    Severity::Rejected,
                    text,
                    id_key,
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
                    id_key,
                    format!(
                        "id {id:?} is nested with step {other:?}: one of these trees contains \
                         the other, so both steps write the same files. A step's id *is* the \
                         tree it writes, and every tree has exactly one writer."
                    ),
                )
                .with_help(
                    "move one of them out from under the other — sibling functions under one \
                     group, like `ingest` and `render_markdown`, are the usual shape",
                ),
            );
            continue;
        }
        if c.entry.group.is_some() && c.entry.name.is_some() {
            diags.push(
                c.diag(
                    Severity::Warning,
                    text,
                    Some("name"),
                    "`name` on a grouped step is not shown: a step's label comes from its \
                     group's name and its function",
                )
                .with_help("name the group instead, in its `[[groups]]` entry"),
            );
        }

        let spec = match spec_of(&c.entry, group_type) {
            Ok(spec) => spec,
            Err(e) => {
                diags.push(c.diag(Severity::Rejected, text, Some("command"), format!("{e:#}")));
                continue;
            }
        };
        seen.insert(id);
        accepted.push((c.entry, spec));
    }
    (accepted, diags)
}

fn is_datalib_step(prog: &str) -> bool {
    prog == "datalib-step" || prog.ends_with("/datalib-step")
}

/// The retired shape: `datalib-step` with the function and the provider on
/// its command line. Returns the subcommand word, for the message.
/// Tokenised the way the migrator does, so the two agree on what the
/// retired shape looks like.
fn retired_subcommand(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace();
    if !words.next().is_some_and(is_datalib_step) {
        return None;
    }
    words
        .next()
        .filter(|w| matches!(*w, "download" | "render" | "grid_index" | "qmd_index"))
}

fn nests_with(a: &str, b: &str) -> bool {
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// The declared group ids, for a diagnostic that has to say what the valid
/// choices were.
fn id_list<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let all: Vec<&str> = ids.collect();
    if all.is_empty() {
        return "(none — this config declares no groups)".to_string();
    }
    all.join(", ")
}

/// The program a step runs when it writes no `command`. Resolved like any
/// other bare name: `binary_dir`, then `PATH`.
pub const BUILTIN_STEP_PROGRAM: &str = "datalib-step";

fn spec_of(e: &StepEntry, group_type: Option<&str>) -> Result<StepSpec> {
    let mut argv = match &e.command {
        Some(command) => shlex::split(command)
            .with_context(|| format!("command {command:?} has unbalanced quoting"))?,
        None => vec![BUILTIN_STEP_PROGRAM.to_string()],
    };
    if argv.is_empty() {
        bail!("empty command");
    }
    if let Some(params) = &e.params {
        let json =
            serde_json::to_string(&params_to_json(params, &e.id)?).context("params → JSON")?;
        argv.push("--params".to_string());
        argv.push(json);
    }
    if !e.inputs.is_empty() {
        argv.push("--inputs".to_string());
        argv.push(serde_json::to_string(&e.inputs).expect("string vec → JSON"));
    }

    let mut spec = StepSpec::new(
        &e.id,
        StepRun::Subprocess {
            argv,
            env: e.env.clone(),
        },
    );
    spec.code_version = e.code_version.clone();
    spec.group = e.group.clone();
    spec.group_type = group_type.map(str::to_string);
    spec.function = e.function.clone();
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
    text: Option<&str>,
) -> (Vec<AppletEntry>, Vec<Diagnostic>) {
    let mut accepted = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for c in candidates {
        let id = c.entry.id.clone();
        if !is_js_identifier(&id) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "id {id:?} must be a JavaScript identifier (letters, digits, _ or $, not \
                     starting with a digit) because it is injected into card source as a \
                     bare name"
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
                spans: BTreeMap::new(),
                dropped: BTreeSet::new(),
                diagnostics: vec![d],
            };
        }
    };

    let mut diags = Vec::new();
    let mut spans: BTreeMap<String, std::ops::Range<usize>> = BTreeMap::new();
    let mut named: BTreeSet<String> = BTreeSet::new();

    let id_key = |v: &toml::Value| v.get("id").and_then(v_str);
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
                // First spelling wins, matching `accept_steps`.
                spans.entry(id).or_insert(span);
            }
            named.extend(v.get("group").and_then(v_str));
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

    let accepted = accept_entries(
        Candidates {
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
            checkpoint_cadence: raw.checkpoint_cadence,
        },
        specs,
        spans,
        dropped,
        diagnostics: diags,
    }
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
    let (graph, mut graph_diags) =
        Graph::build_graded(std::mem::take(&mut entries.specs), &entries.dropped);

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
    /// What a fatal diagnostic leaves behind. Deliberately not `Default`:
    /// "empty" here is a failure state and should read as one.
    fn empty() -> Self {
        DagConfig {
            data_root: None,
            binary_dir: None,
            groups: Vec::new(),
            steps: Vec::new(),
            applets: Vec::new(),
            checkpoint_cadence: None,
        }
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::CheckpointCadence;

    #[test]
    fn a_cadence_survives_the_trip_through_the_env_var() {
        let c = CheckpointCadence {
            quiet_for_secs: 2.5,
            at_most_every_secs: 15.0,
        };
        assert_eq!(CheckpointCadence::decode(&c.encode()), Some(c));
    }

    /// A value this build cannot read is `None`, never a guess: the step
    /// falls back to its own default and logs, rather than checkpointing on
    /// a cadence nobody asked for.
    #[test]
    fn an_unreadable_cadence_is_none() {
        for bad in [
            "",
            "2",
            "2,",
            ",15",
            "two,fifteen",
            "-1,15",
            "2,-1",
            "nan,15",
            "inf,15",
        ] {
            assert_eq!(
                CheckpointCadence::decode(bad),
                None,
                "{bad:?} must not parse"
            );
        }
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

    /// A built-in step writes no `command`: its argv is `datalib-step`
    /// plus the declared fields as JSON flags, and the function, group and
    /// type it dispatches on travel in the environment instead.
    #[test]
    fn a_builtin_step_runs_datalib_step_with_declared_fields_as_json_flags() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[groups]]
            id = "slack"
            type = "slack"

            [[groups]]
            id = "unified_index"

            [[steps]]
            group = "slack"
            function = "ingest"
            params.api = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            group = "slack"
            function = "render_markdown"
            inputs = ["slack/ingest"]
            params.api = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            group = "unified_index"
            function = "grid_index"
            inputs = ["slack/render_markdown"]
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        assert_eq!(specs.len(), 3);

        let argv = |i: usize| match &specs[i].run {
            StepRun::Subprocess { argv, .. } => argv.clone(),
            other => panic!("expected subprocess, got {other:?}"),
        };
        let dl = argv(0);
        assert_eq!(dl[0], BUILTIN_STEP_PROGRAM);
        assert_eq!(dl[1], "--params");
        let params: serde_json::Value = serde_json::from_str(&dl[2]).unwrap();
        assert_eq!(params["api"]["channels"][0], "chat-qi");
        // No inputs declared → no --inputs, and nothing else: the one tree
        // a step writes is its id, which it reads from the environment,
        // and so are the function and the provider.
        assert_eq!(dl.len(), 3);

        // TOML has no anchors, so the render step repeats the subtree —
        // and must produce byte-identical JSON for it.
        let rn = argv(1);
        assert_eq!(rn[2], dl[2]);
        assert_eq!(&rn[3..], &["--inputs", r#"["slack/ingest"]"#]);

        // Param-less step: just inputs.
        assert_eq!(
            argv(2),
            vec![
                BUILTIN_STEP_PROGRAM,
                "--inputs",
                r#"["slack/render_markdown"]"#,
            ]
        );

        // The composed ids are what everything downstream sees.
        assert_eq!(specs[0].id, "slack/ingest");
        assert_eq!(specs[2].id, "unified_index/grid_index");
        assert_eq!(specs[0].group.as_deref(), Some("slack"));
        assert_eq!(specs[0].group_type.as_deref(), Some("slack"));
        assert_eq!(specs[0].function.as_deref(), Some("ingest"));
        assert_eq!(specs[2].group_type, None);
    }

    #[test]
    fn a_groups_type_moves_the_fingerprint_and_its_name_does_not() {
        let with = |group_line: &str| {
            let cfg: DagConfig = toml::from_str(&format!(
                r#"
                [[groups]]
                id = "mail"
                {group_line}

                [[steps]]
                group = "mail"
                function = "raw"
                command = "my-fetcher"
                "#
            ))
            .expect("parse");
            to_specs(&cfg).expect("to_specs").remove(0)
        };
        let untyped = with("");
        let email = with(r#"type = "email""#);
        let slack = with(r#"type = "slack""#);
        let named = with("type = \"email\"\nname = \"Fastmail\"");
        assert_ne!(untyped.fingerprint_material(), email.fingerprint_material());
        assert_ne!(email.fingerprint_material(), slack.fingerprint_material());
        assert_eq!(email.fingerprint_material(), named.fingerprint_material());
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

    /// Two sources of the same type, which is what "Add Data Source"
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
    fn rejects_ids_under_system() {
        for id in ["system", "system/state", "system/a/b"] {
            let cfg: DagConfig =
                toml::from_str(&format!(r#"steps = [{{id = "{id}", command = "a"}}]"#)).unwrap();
            let err = to_specs(&cfg).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{id}: {err}");
        }
    }

    /// `unified_index` needs no reserved-name rule: the index steps' ids *are*
    /// those trees, so anything else claiming one is an ordinary duplicate.
    #[test]
    fn the_index_steps_own_unified_index_by_being_it() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/rendered_md", command = "r"},
                 {id = "unified_index/grid", command = "a", inputs = ["slack/rendered_md"]},
                 {id = "unified_index/qmd", command = "b", inputs = ["slack/rendered_md"]},
               ]"#,
        )
        .unwrap();
        let specs = to_specs(&cfg).expect("index steps must remain valid");
        crate::Graph::build(specs).expect("index steps must graph");

        // And a second claimant is refused as a duplicate, with no
        // special-case list involved.
        let clash: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "unified_index/grid", command = "a"},
                 {id = "unified_index/grid", command = "b"},
               ]"#,
        )
        .unwrap();
        assert!(to_specs(&clash).is_err());
    }

    /// No `command` means `datalib-step`, and `datalib-step` needs a group
    /// to know what to do — so the one shape that gets neither is refused.
    #[test]
    fn a_step_with_no_command_needs_a_group() {
        let cfg: DagConfig = toml::from_str(r#"steps = [{id = "x/out"}]"#).unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("command"), "{err}");
        assert!(err.contains("group"), "{err}");
    }

    /// A step is `group` + `function` or a verbatim `id`, and the loader
    /// says which half is missing rather than accepting a half-step.
    #[test]
    fn a_step_is_group_and_function_or_a_verbatim_id() {
        let err = |body: &str| {
            toml::from_str::<DagConfig>(&format!("[[steps]]\n{body}\ncommand = \"x\"\n"))
                .unwrap_err()
                .to_string()
        };
        assert!(err("group = \"a\"").contains("missing field `function`"));
        assert!(err("function = \"raw\"").contains("missing field `group`"));
        assert!(err("").contains("missing field `group`"));
        let both = err("id = \"a/raw\"\ngroup = \"a\"\nfunction = \"raw\"");
        assert!(both.contains("`id` cannot be written"), "{both}");

        let ok: DagConfig =
            toml::from_str("[[steps]]\ngroup = \"a\"\nfunction = \"raw\"\ncommand = \"x\"\n")
                .unwrap();
        assert_eq!(ok.steps[0].id, "a/raw");
        let custom: DagConfig =
            toml::from_str("[[steps]]\nid = \"tools/csv\"\ncommand = \"x\"\n").unwrap();
        assert_eq!(custom.steps[0].id, "tools/csv");
        assert_eq!(custom.steps[0].group, None);
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
        let StepRun::Subprocess { argv, .. } = &specs[0].run else {
            panic!("expected subprocess");
        };
        let params: serde_json::Value = serde_json::from_str(&argv[2]).unwrap();
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
id = "unified_index/grid_index"
command = "index-it"
inputs = ["pdfs/ingest"]

[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"
"#,
        );
        // The broken step is gone and named; everything else loaded.
        assert_eq!(ids(&check), vec!["pdfs/ingest", "unified_index/grid_index"]);
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
            ("unified_index", "unified_index/grid"),
            ("unified_index/grid", "unified_index"),
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
    /// depends on it: `unified_index` the applet sits beside
    /// `unified_index/grid_index` the step.
    #[test]
    fn the_scaffold_shape_loads_clean() {
        let check = check_text(
            r#"
[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"

[[steps]]
group = "unified_index"
function = "qmd_index"

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
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
id = "unified_index/grid"
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
            sev_of(&check.diagnostics, "unified_index/grid"),
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
            .find(|d| d.id() == Some("unified_index/grid"))
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
        assert_eq!(check.cfg.steps[0].command.as_deref(), Some("first"));
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
id = "unified_index"
command = "datalib-applet unified_index"

[[applets]]
id = "2fa"
command = "x"
"#,
        );
        assert_eq!(ids(&check), vec!["good/raw"]);
        assert_eq!(check.cfg.applets.len(), 1);
        assert_eq!(check.cfg.applets[0].id, "unified_index");
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
group = "work-slack"
function = "ingest"

[[steps]]
group = "work-slack"
function = "render_markdown"
inputs = ["work-slack/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["work-slack/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
"#;

    /// The plan's example shape loads clean, and the ids everything
    /// downstream keys on are the composed ones.
    #[test]
    fn the_grouped_shape_loads_clean_with_composed_ids() {
        let check = check_text(GROUPED);
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        let mut ids: Vec<&str> = check.cfg.steps.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "unified_index/grid_index",
                "work-slack/ingest",
                "work-slack/render_markdown"
            ]
        );
        assert_eq!(check.cfg.groups.len(), 2);
        assert_eq!(check.cfg.groups[0].name.as_deref(), Some("Work Slack"));
        assert_eq!(check.cfg.applets[0].group.as_deref(), Some("unified_index"));
        assert!(check.graph.by_id.contains_key("work-slack/render_markdown"));
    }

    /// A step naming a group that does not exist is unusable — its id
    /// cannot be composed against anything — and the message lists what
    /// does exist.
    #[test]
    fn a_step_under_an_undeclared_group_is_rejected() {
        let check = check_text(
            "[[groups]]\nid = \"slack\"\n\n\
             [[steps]]\ngroup = \"slakc\"\nfunction = \"raw\"\ncommand = \"x\"\n",
        );
        let d = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("slakc/raw"))
            .expect("a diagnostic for the step");
        assert_eq!(d.severity, Severity::Rejected);
        assert!(d.message.contains("names no declared group"), "{d:?}");
        assert!(d.help.as_deref().unwrap().contains("slack"), "{d:?}");
        assert_eq!(d.line, Some(5), "the `group =` line");
        assert!(check.cfg.steps.is_empty());
    }

    /// A bad group id costs the group and every step under it — and those
    /// steps are told the fix is on the group, not on them.
    #[test]
    fn a_bad_group_takes_its_steps_with_it_as_blocked() {
        let check = check_text(
            "[[groups]]\nid = \"a/b\"\n\n\
             [[steps]]\ngroup = \"a/b\"\nfunction = \"raw\"\ncommand = \"x\"\n\n\
             [[steps]]\nid = \"fine/raw\"\ncommand = \"y\"\n",
        );
        assert_eq!(sev_of(&check.diagnostics, "a/b"), Some(Severity::Rejected));
        let step = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("a/b/raw"))
            .unwrap();
        assert_eq!(step.severity, Severity::Blocked);
        assert!(step.message.contains("was itself dropped"), "{step:?}");
        assert!(check.cfg.groups.is_empty());
        let ids: Vec<&str> = check.cfg.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["fine/raw"]);
    }

    #[test]
    fn a_group_id_is_one_segment_and_never_system() {
        for bad in ["", "a/b", "..", "-x", "a b", "system"] {
            let check = check_text(&format!("[[groups]]\nid = \"{bad}\"\n"));
            assert_eq!(check.cfg.groups.len(), 0, "{bad:?} should be rejected");
            assert_eq!(check.diagnostics[0].severity, Severity::Rejected);
        }
        for good in ["a", "work-slack", "a.b_c", "unified_index"] {
            let check = check_text(&format!(
                "[[groups]]\nid = \"{good}\"\n[[steps]]\ngroup = \"{good}\"\n\
                 function = \"raw\"\ncommand = \"x\"\n"
            ));
            assert!(check.is_clean(), "{good:?}: {:?}", check.diagnostics);
        }
    }

    #[test]
    fn a_function_is_one_segment() {
        for bad in ["", "a/b", "..", "-x"] {
            let check = check_text(&format!(
                "[[groups]]\nid = \"g\"\n[[steps]]\ngroup = \"g\"\nfunction = \"{bad}\"\n\
                 command = \"x\"\n"
            ));
            assert!(check.cfg.steps.is_empty(), "{bad:?} should be rejected");
            let d = check
                .diagnostics
                .iter()
                .find(|d| d.severity == Severity::Rejected)
                .unwrap();
            assert!(d.message.contains("function"), "{d:?}");
        }
    }

    /// Two groups with one id would both claim every step under it; the
    /// first keeps the id, as with steps.
    #[test]
    fn the_first_of_two_duplicate_groups_wins() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\nname = \"first\"\n\n[[groups]]\nid = \"g\"\nname = \"second\"\n\n\
             [[steps]]\ngroup = \"g\"\nfunction = \"raw\"\ncommand = \"x\"\n",
        );
        assert_eq!(check.cfg.groups.len(), 1);
        assert_eq!(check.cfg.groups[0].name.as_deref(), Some("first"));
        assert_eq!(check.cfg.steps.len(), 1, "the step still resolves");
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].severity, Severity::Rejected);
    }

    /// A composed id and a verbatim one share one namespace: a custom step
    /// cannot claim a tree a grouped step writes, and the collision is
    /// reported the same way whichever was written first.
    #[test]
    fn composed_and_verbatim_ids_collide_in_one_namespace() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\n\n\
             [[steps]]\ngroup = \"g\"\nfunction = \"raw\"\ncommand = \"x\"\n\n\
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

    /// The two warnings: a group nothing is filed under, and a name written
    /// on a grouped step. Neither drops anything.
    #[test]
    fn empty_groups_and_step_names_warn_without_dropping() {
        let check = check_text(
            "[[groups]]\nid = \"lonely\"\n\n[[groups]]\nid = \"g\"\n\n\
             [[steps]]\ngroup = \"g\"\nfunction = \"raw\"\nname = \"Nope\"\ncommand = \"x\"\n",
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
        let named = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("g/raw"))
            .unwrap();
        assert_eq!(named.severity, Severity::Warning);
        assert_eq!(named.line, Some(10), "the `name =` line");
    }

    /// A group whose only step was rejected is not empty — somebody filled
    /// it, and the entry to fix is the step.
    #[test]
    fn a_group_whose_step_was_rejected_is_not_called_empty() {
        let check = check_text(
            "[[groups]]\nid = \"g\"\n\n\
             [[steps]]\ngroup = \"g\"\nfunction = \"raw\"\ncommand = \"x\"\ntitle = 1\n",
        );
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].id(), Some("g/raw"));
    }

    /// An applet's group is a filing, so a wrong one is advice rather than a
    /// reason to take the grid down.
    #[test]
    fn an_applet_under_an_undeclared_group_only_warns() {
        let check =
            check_text("[[applets]]\ngroup = \"nope\"\nid = \"unified_index\"\ncommand = \"x\"\n");
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
        assert_eq!(spec.function, None);
    }

    /// The shape written before `datalib-step` read its function from the
    /// environment cannot run any more, so it is refused rather than run
    /// into an "unrecognized subcommand" at sync time — and the refusal
    /// names the migrator, whether the step was grouped or not.
    #[test]
    fn the_retired_subcommand_shape_is_rejected_and_names_the_migrator() {
        for text in [
            "[[steps]]\nid = \"slack/raw\"\ncommand = \"datalib-step download slack_api\"\n",
            "[[groups]]\nid = \"slack\"\ntype = \"slack_api\"\n\n\
             [[steps]]\ngroup = \"slack\"\nfunction = \"raw\"\ncommand = \"datalib-step download slack_api\"\n",
            "[[groups]]\nid = \"unified_index\"\n\n\
             [[steps]]\ngroup = \"unified_index\"\nfunction = \"grid\"\ncommand = \"datalib-step grid_index\"\n",
        ] {
            let check = check_text(text);
            assert_eq!(check.dropped(), 1, "{text}\n{:?}", check.diagnostics);
            let d = check
                .diagnostics
                .iter()
                .find(|d| d.severity == Severity::Rejected)
                .expect("rejected");
            assert!(d.describe().contains("datalib-migrate-config"), "{}", d.describe());
        }
        // An explicit `datalib-step` with no subcommand is the built-in
        // step spelled out, and fine.
        let check = check_text(
            "[[groups]]\nid = \"slack\"\ntype = \"slack_api\"\n\n\
             [[steps]]\ngroup = \"slack\"\nfunction = \"ingest\"\ncommand = \"datalib-step --playback-root /tmp/pb\"\n",
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
    }
}
