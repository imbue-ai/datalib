//! The DAG config file, `config.toml` — the format that replaced the
//! old stanza-based `sources:` config. The user declares the steps
//! directly; edges are still derived from artifact-path overlap, never
//! written by hand.
//!
//! ```toml
//! data_root = "~/datalib-data"     # default: the config file's dir
//! binary_dir = "/opt/datalib/bin"  # optional: prepended to PATH
//!
//! [[steps]]
//! id = "slack.download"
//! name = "Work Slack"             # optional, and read only by the UI
//! command = "datalib-step download slack_api"
//! outputs = ["slack/raw"]
//! # `params` is the provider's own config subtree. As a sub-table it
//! # must come after this step's plain keys — a TOML header ends the
//! # table it appears in.
//! [steps.params.sync]
//! channels = ["chat-qi"]
//!
//! [[steps]]
//! id = "grid_index"
//! command = "datalib-step grid_index"
//! inputs = ["**/rendered_md"]
//! outputs = ["unified_index/grid"]
//!
//! [[steps]]
//! id = "custom"
//! command = "my-exporter --flag"   # any executable on PATH
//! outputs = ["custom/out"]
//!
//! [[applets]]
//! id = "slack_view"                # a JS identifier: it reaches
//!                                  # card source as a bare name
//! command = "datalib-applet slack"
//! [applets.params]
//! tree = "slack/rendered_md"
//! ```
//!
//! There are two kinds of entry. A **step** is scheduled: it reads and
//! writes artifacts, and the DAG is derived from those. An **applet**
//! is never scheduled — it is a long-lived server the http gateway
//! spawns on demand, contributing frontend components plus the
//! endpoints behind them, and it declares no inputs/outputs because it
//! owns no artifacts. See `docs/dev/applets.md`. This module parses and
//! validates both; only steps reach the scheduler.
//!
//! Note that top-level `data_root` / `binary_dir` must be written
//! *above* the first `[[steps]]`, since everything after a table
//! header belongs to that table.
//!
//! A step body is a `command` — a single string split shell-style
//! (quotes and backslash escapes, but no variable expansion or
//! globbing; wrap in `sh -c '…'` for real shell). The declared
//! `params` / `inputs` / `outputs` are appended to the argv as
//! `--params JSON` / `--inputs JSON` / `--outputs JSON`, each only
//! when present, so the command needs no TOML parser and the argv
//! stays reproducible. Any executable that understands those flags
//! (and optionally the NDJSON stdout protocol) can be a step — see
//! docs/dev/step_protocol.md.
//!
//! A step's `id` is its identity: unique, path-safe, and the string the
//! directory structure is formed from, which makes changing it a
//! migration rather than an edit. `name` carries the half that is safe
//! to change — see [`StepEntry::name`].
//!
//! TOML has no anchors, so a params subtree shared between a download
//! and a render step is written out twice. In practice the two halves
//! want different knobs anyway, so this is rarely the duplication it
//! looks like.
//!
//! This is the *only* config format the runner accepts. Data roots
//! written before the TOML switch are converted once, out of band, by
//! the separate `datalib-migrate-config` program — which is where every
//! legacy schema and the last YAML parser live.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::diagnostics::{Diagnostic, EntryRef, Severity};
use crate::graph::Graph;
use crate::step::{StepRun, StepSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagConfig {
    /// Root for all artifacts. Optional: defaults to the directory the
    /// config file lives in, so a data root containing its own config
    /// is self-contained (same rule as the old format).
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    /// Directory prepended to `PATH` for every step subprocess, so
    /// commands can name binaries bare (`datalib-step …`). Optional;
    /// see [`resolve_binary_dir`] for the fallback chain.
    #[serde(default)]
    pub binary_dir: Option<PathBuf>,
    /// Defaults to empty: a config with no steps yet (a bare
    /// `data_root:` file) is valid — it just runs nothing.
    #[serde(default)]
    pub steps: Vec<StepEntry>,
    /// Long-lived servers that contribute the app's frontend and its
    /// data endpoints. Unlike steps these are never scheduled: the
    /// http gateway spawns one on demand when a request for its
    /// prefix arrives. Empty is normal — a data root with no applets
    /// still syncs and still serves the builtin UI.
    #[serde(default)]
    pub applets: Vec<AppletEntry>,
}

/// One applet instance. Deliberately a subset of [`StepEntry`]: an
/// applet declares no `inputs`/`outputs` because it is not scheduled
/// and owns no artifacts of its own — it reads what steps already
/// wrote. Everything else (`command` splitting, `params` as JSON,
/// `env` merge, cwd = data root) follows the step's conventions so
/// there is one set of rules to learn.
///
/// There is no `title`, and `deny_unknown_fields` means a config
/// carrying one is rejected by name. The field existed and nothing
/// read it: the label the component gallery shows is written by the
/// applet itself into its namespace metadata, so a config-level title
/// was a second, silent spelling of a label that lives elsewhere. An
/// applet that wants one takes it through `params` — the slack
/// applet's `workspace`, for instance.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletEntry {
    /// Instance name. Doubles as the mount prefix (`/applet/<id>/`) *and*
    /// as an identifier injected into card-source scope, so it is
    /// restricted to what JavaScript will accept as a variable name —
    /// see [`validate_applets`].
    pub id: String,
    /// The command to run, split shell-style into an argv, resolved
    /// the same way a step's is (`binary_dir`, then `PATH`).
    pub command: String,
    /// Arbitrary applet parameters, forwarded verbatim as JSON via
    /// `--params` — to both the manifest dump and the server.
    #[serde(default)]
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl AppletEntry {
    /// `params` as JSON, ready for `--params`. `None` when the entry
    /// declared none.
    pub fn params_json(&self) -> Result<Option<serde_json::Value>> {
        match &self.params {
            Some(v) => Ok(Some(params_to_json(v, &self.id)?)),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    pub id: String,
    /// What to call this step on screen. Free text, freely changed.
    ///
    /// The runner never reads it: it is not passed to the child, and it
    /// is deliberately absent from [`StepSpec::fingerprint_material`],
    /// so renaming a step does not make it stale and does not re-run
    /// anything. Its consumers are both grids — the Pipeline table
    /// shows it in place of the step's `id`, and the unified index
    /// grid shows it beside the rows that step produced
    /// (`datalib/ui/src/config/sourceSteps.ts`).
    ///
    /// **`name` and `id` are the two halves of one identity, and only
    /// this half is malleable.** The `id` is the identity: it is
    /// path-safe, unique, and the directory structure is formed from
    /// it, so changing it moves data on disk and strands the paths the
    /// index recorded — a migration, not an edit. The `name` is what a
    /// person types and what they see; it carries no meaning to any
    /// program. The wizard derives an `id` from the `name` once, at
    /// creation, and never again.
    ///
    /// Written only when it differs from the `id`. A name that merely
    /// respells the id would be the second, silent spelling of one
    /// string, which is what got the applet `title` key deleted
    /// (00633dd5) — so an unnamed step is displayed by its id, and its
    /// config stays as it was.
    ///
    /// Any step may carry one: the shared `grid_index` / `qmd_index`
    /// fan-ins are rows in the same table and are named the same way.
    /// [`AppletEntry`] deliberately has no counterpart — an applet's
    /// own `params` already carry whatever label it wants (see that
    /// type's docs), and it is displayed by its `id`.
    #[serde(default)]
    pub name: Option<String>,
    /// The ids of the steps this one reads.
    ///
    /// A step id *is* the tree that step writes, so an entry here is
    /// simultaneously a step reference and an artifact path — which is
    /// why there is nothing to match and nothing to glob. Every entry
    /// must name a declared step; a directory staged by hand is named
    /// by `params.common.input_path` instead, and is not an artifact
    /// the DAG knows about.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The command to run, split shell-style into an argv. Note the
    /// child runs with its cwd set to `data_root`, so a relative
    /// multi-component argv[0] resolves against the data root; use a
    /// bare name (PATH — see `binary_dir`) or an absolute path for
    /// binaries that live elsewhere.
    pub command: String,
    /// Arbitrary step parameters, forwarded verbatim as JSON via
    /// `--params`.
    #[serde(default)]
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional version of the step's own behavior, for steps whose
    /// output can change without their command line changing (a
    /// renderer that was reworked, a binary that was upgraded in
    /// place). Bumping it makes the runner re-run the step once, even
    /// though none of its inputs moved.
    #[serde(default)]
    pub code_version: Option<String>,
}

/// The config file inside a data root: `<data_root>/config.toml`. The
/// app reads and writes only this, which is what makes a data root
/// self-contained.
pub fn root_config_path(data_root: &Path) -> PathBuf {
    data_root.join(CONFIG_FILE_NAME)
}

/// The canonical config filename.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Parse config text, strictly: any problem at all is an error.
///
/// The strict view of [`parse_graded`]. Two callers want exactly this
/// — `datalib-migrate-config`, which must not emit a config with a
/// known problem in it, and `PUT /api/config`, which refuses to write
/// one. The error is the first diagnostic, with its line; call
/// `parse_graded` when you want all of them, which is what anything
/// reporting to a human should do.
pub fn parse(text: &str) -> Result<DagConfig> {
    let (cfg, diagnostics) = parse_graded(text);
    if let Some(d) = diagnostics.first() {
        bail!("{}", d.describe());
    }
    Ok(cfg)
}

/// Whether this text is TOML that could be a config at all.
///
/// The file-level question, and only that: it has no opinion on
/// whether the config the file spells is *valid*, which is
/// [`check_text`]'s job. `datalib-migrate-config` asks it to notice a
/// config that has already been converted, and that guard has to hold
/// for a converted config with a problem in it too — otherwise the
/// second run of the migrator falls through to the YAML parser and
/// produces exactly the baffling error the guard exists to prevent.
pub fn is_toml(text: &str) -> bool {
    !parse_graded(text)
        .1
        .iter()
        .any(|d| d.severity == Severity::Fatal)
}

/// Load + resolve a config file, strictly. `data_root` defaults to the
/// config file's directory and gets `~` expanded.
///
/// The strict view of [`load_graded`], and the same trade: one error
/// instead of every problem. Prefer `load_graded` anywhere the result
/// reaches a person.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg = parse(&text).with_context(|| format!("parse {}", path.display()))?;
    let root = data_root_of(path, &cfg);
    Ok((cfg, root))
}

/// Read and check a config file, keeping whatever loads.
///
/// Only I/O failures are `Err`: a file that cannot be read has no
/// diagnostics to give. Everything the file itself gets wrong comes
/// back in [`ConfigCheck::diagnostics`], including the case where it
/// is not a config at all.
pub fn load_graded(path: &Path) -> Result<(ConfigCheck, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let checked = check_text(&text);
    let root = data_root_of(path, &checked.cfg);
    Ok((checked, root))
}

/// Where this config's artifacts live: its own `data_root`, else the
/// directory the config file sits in — which is what makes a data root
/// holding its own config self-contained.
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

/// The one reserved top-level directory: the runner's and the server's
/// own state (`system/dag_state.json`, `system/jobs.doltlite_db`, the
/// job logs). A step writing there would put the scheduler's own
/// bookkeeping under its change detection.
///
/// `unified_index` used to be reserved alongside it, back when a
/// source's identity was the first segment of a free-form output path
/// and nothing stopped a stanza from claiming that segment. It needs no
/// rule now: the index steps' ids *are* `unified_index/grid` and
/// `unified_index/qmd`, and id uniqueness does the rest.
///
/// This is the *policy*. The path constants live in
/// `datalib_core::layout`, which this crate deliberately doesn't depend
/// on — the runner is lean on purpose. `layout.rs` points here.
pub const SYSTEM_DIR: &str = "system";

/// One id segment: what a directory name may contain.
///
/// Deliberately narrower than the filesystem allows. An id is a path
/// component on every platform we ship to, it appears inside
/// `markdowns.md_path` and `grid_rows.qmd_path`, and it is compared as
/// a whole string everywhere — so the portable-filename character set
/// plus `.` is all it needs to be.
fn valid_id_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && !seg.starts_with('-')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Validate the `[[steps]]` array as a whole, strictly — the checks
/// that need to see every entry, not just one.
///
/// The strict view of [`accept_steps`], which is where the rules
/// actually live and which is documented with them. Kept because
/// [`to_specs`] and its callers want the first problem as an `Err`.
pub fn validate_steps(cfg: &DagConfig) -> Result<()> {
    let (_, diags) = accept_steps(
        candidates(&cfg.steps, EntryRef::step, |e| Some(e.id.clone())),
        None,
    );
    if let Some(d) = diags.first() {
        bail!("{}", d.describe());
    }
    Ok(())
}

/// Turn config entries into scheduler specs, strictly.
///
/// The strict view of [`accept_steps`]. Note what this does *not* do:
/// it resolves nothing between steps, so an `inputs` entry naming no
/// declared step passes here and is caught by [`crate::Graph::build`],
/// which is the first place the full set of ids exists.
pub fn to_specs(cfg: &DagConfig) -> Result<Vec<StepSpec>> {
    let (accepted, diags) = accept_steps(
        candidates(&cfg.steps, EntryRef::step, |e| Some(e.id.clone())),
        None,
    );
    if let Some(d) = diags.first() {
        bail!("{}", d.describe());
    }
    Ok(accepted.into_iter().map(|(_, spec)| spec).collect())
}

/// A step's `params` subtree as the JSON the child gets on `--params`.
///
/// Serializing `toml::Value` straight through serde would be wrong:
/// TOML's date/time types have no JSON counterpart, and the `toml`
/// crate smuggles them past a non-TOML serializer as a one-key map
/// (`{"$__toml_private_datetime": …}`), which is what the step would
/// then try to deserialize. So walk the tree and render every datetime
/// as its RFC-3339-ish string — `since = 2026-06-15` and
/// `since = "2026-06-15"` reach the step identically, which is what a
/// user writing either one expects.
fn params_to_json(v: &toml::Value, step: &str) -> Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match v {
        toml::Value::String(s) => J::String(s.clone()),
        toml::Value::Integer(i) => J::from(*i),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => J::Number(n),
            // TOML has nan/inf literals; JSON has no way to say them,
            // so refuse here rather than hand the child a null it
            // would read as "unset".
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

/// Locate the directory prepended to every step's `PATH`. Precedence:
/// CLI override (`--binary-dir`), then config `binary_dir:`, then the
/// running executable's own directory — a packaged release lays the
/// step binaries next to the runner. `None` only when even the
/// executable path is unknowable; steps then get the inherited `PATH`
/// untouched.
///
/// Relative paths are absolutized against the *runner's* cwd, because
/// steps are spawned with their cwd set to `data_root` — a relative
/// `--binary-dir bazel-bin/...` would otherwise be re-resolved against
/// the data root.
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

/// The one namespace an applet may not claim.
///
/// Frontend components all live under `system/frontend/<namespace>/`,
/// and starting an applet *deletes* its namespace directory first, so
/// it rewrites the whole thing. `user` holds hand- and agent-authored
/// components, which nothing regenerates — so an applet allowed to take
/// that id would have its directory wiped the next time it started,
/// taking the user's own work with it.
pub const RESERVED_APPLET_ID: &str = "user";

/// Check the applet list before anything tries to use it, strictly.
///
/// The strict view of [`accept_applets`], where the rules and the
/// reason for each of them live.
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

/// Conservative subset of what JavaScript accepts: ASCII only. The
/// language would allow plenty of Unicode, but an id is also a URL
/// path segment and a directory-safe token elsewhere, so the narrow
/// rule is the useful one.
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

// ---------------------------------------------------------------------------
// The graded loader
// ---------------------------------------------------------------------------
//
// Everything above this line is the strict door: first problem wins and
// the caller gets an `Err`. Everything below is the graded one, which
// keeps what loads and reports the rest. Both run the same rules — the
// strict functions are thin wrappers, so there is no second spelling of
// what a valid config is. See `crate::diagnostics` for what the
// severities mean and why there are four of them.

/// The file's own shape, with the entries left opaque.
///
/// This split is what makes a graded load possible. Deserializing
/// straight into [`DagConfig`] makes serde's first objection — an
/// unknown key three steps down — the whole file's error, because
/// serde has no way to say "skip that one and keep going". Taking the
/// entries as `toml::Value` first, and deserializing each on its own,
/// turns that objection back into what it is: one entry's problem.
///
/// `deny_unknown_fields` stays here and stays fatal, because at *this*
/// level it means something different: an unknown top-level key is a
/// statement about the file, and there is no smaller thing to drop.
///
/// `Spanned` wraps the entries and not their fields. The location a
/// reader wants is the `[[steps]]` header the entry begins at; a
/// per-field span would only ever point somewhere that header already
/// leads, at the cost of `Spanned` infecting every field of
/// [`StepEntry`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    data_root: Option<PathBuf>,
    #[serde(default)]
    binary_dir: Option<PathBuf>,
    #[serde(default)]
    steps: Vec<toml::Spanned<toml::Value>>,
    #[serde(default)]
    applets: Vec<toml::Spanned<toml::Value>>,
}

/// One entry on its way in: where it sits in the file, and what it
/// deserialized to.
///
/// Generic over the entry type so steps and applets share the
/// bookkeeping. The *rules* are not shared, because they are not the
/// same rules — an applet owns no artifacts, so nothing about trees,
/// nesting or `system/` applies to it.
struct Candidate<T> {
    entry: T,
    reference: EntryRef,
    /// Byte range of the entry's header. `None` when the caller came
    /// in through the strict, text-less door ([`to_specs`]), which
    /// holds a `DagConfig` and no file to point into.
    span: Option<std::ops::Range<usize>>,
}

impl<T> Candidate<T> {
    /// A diagnostic about this entry, located when we have a location.
    ///
    /// `key` narrows the location from the entry's header to the one
    /// key the complaint is about — which is where a reader is looking
    /// and where the UI editor should put its highlight. Pass `None`
    /// when the entry as a whole is the problem.
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

/// Wrap already-deserialized entries as candidates with no spans — the
/// adapter that lets the strict, `DagConfig`-shaped callers run the
/// very same rules as the graded one.
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

/// Every step rule that can be decided from the `[[steps]]` array
/// alone, applied entry by entry, dropping what fails and saying why.
///
///   * **Ids are well-formed.** An id is the tree the step writes, so
///     it has to be a usable relative path: non-empty segments from the
///     portable filename character set, no `.`/`..`, no leading `-`.
///   * **Ids are unique.** They key the persisted scheduler state
///     (`DagState.steps`, a map), so two entries sharing an id get one
///     bookkeeping slot between them and clobber each other's
///     up-to-date bookkeeping in turn — while both still run, against
///     the same tree. TOML cannot enforce it for us, since `[[steps]]`
///     is an array.
///   * **No id is nested inside another.** `unified_index` and
///     `unified_index/grid` are two steps writing one tree, which is
///     the same violation as a duplicate — and it was silently accepted
///     until #209, because uniqueness was checked as string equality
///     while a step's id is a *path*. This is the load-bearing one:
///     two writers under one tree is two writers on one
///     `.doltlite_db`, whose working set is shared across processes,
///     so they commit each other's in-flight rows. Corruption with no
///     failed step and no log line.
///   * **Nothing writes under `system/`.** See [`SYSTEM_DIR`].
///   * **The command is runnable** — it splits shell-style and is not
///     empty — **and `params` can be JSON**, since that is how the
///     child receives them.
///
/// Later entries lose to earlier ones, so a config's first spelling of
/// an id survives and the diagnostic names both.
///
/// Inputs are deliberately *not* checked here: an input names another
/// step, and [`crate::Graph::build_graded`] is the first place the full
/// set of surviving ids exists.
fn accept_steps(
    candidates: Vec<Candidate<StepEntry>>,
    text: Option<&str>,
) -> (Vec<(StepEntry, StepSpec)>, Vec<Diagnostic>) {
    let mut accepted: Vec<(StepEntry, StepSpec)> = Vec::with_capacity(candidates.len());
    let mut diags = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for c in candidates {
        let id = c.entry.id.clone();
        if id.is_empty() || !id.split('/').all(valid_id_segment) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
                format!(
                    "id {id:?} is not a usable directory name. An id is the directory the \
                     step writes, so every `/`-separated segment must be a portable \
                     filename — letters, digits, `.`, `_`, `-`, not starting with `-`, and \
                     never `.` or `..`."
                ),
            ));
            continue;
        }
        if id == SYSTEM_DIR || id.starts_with(&format!("{SYSTEM_DIR}/")) {
            diags.push(c.diag(
                Severity::Rejected,
                text,
                Some("id"),
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
        // Containment, which the string equality above cannot see.
        // Checked both ways: this id may sit inside an accepted one, or
        // an accepted one may sit inside this id. Either is two steps
        // writing one tree.
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
                    "move one of them out from under the other — sibling ids like \
                     `name/raw` and `name/rendered_md` are the usual shape",
                ),
            );
            continue;
        }

        let spec = match spec_of(&c.entry) {
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

/// Whether two step ids name trees where one contains the other.
///
/// Equal ids answer `false`: that is a duplicate, a different rule with
/// a different message, and it is checked first.
fn nests_with(a: &str, b: &str) -> bool {
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// One entry's `command`/`params`/`inputs` as the spec the scheduler
/// runs: split the command shell-style and append the declared
/// params/inputs/outputs as `--flag JSON` pairs (each only when
/// present).
fn spec_of(e: &StepEntry) -> Result<StepSpec> {
    let mut argv = shlex::split(&e.command)
        .with_context(|| format!("command {:?} has unbalanced quoting", e.command))?;
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
    // The step protocol is unchanged: a child still receives
    // `--outputs`, now with the single tree its id names. Steps written
    // against the old contract keep working without knowing the config
    // stopped declaring it.
    argv.push("--outputs".to_string());
    argv.push(serde_json::to_string(&[&e.id]).expect("string vec → JSON"));

    let mut spec = StepSpec::new(
        &e.id,
        StepRun::Subprocess {
            argv,
            env: e.env.clone(),
        },
    );
    spec.code_version = e.code_version.clone();
    for i in &e.inputs {
        spec.inputs.push(crate::ArtifactPath::parse(i)?);
    }
    Ok(spec)
}

/// The applet rules, applied entry by entry. Three, all load-bearing
/// rather than stylistic:
///
///   * **Ids are JavaScript identifiers.** An applet id is injected
///     into card-source scope as a bare name (`slack_work.channels()`),
///     and card source is evaluated by `new Function`, so an id like
///     `slack.work` or `2fa` would be a syntax error at the point a
///     card renders — far from the config that caused it.
///   * **Ids are unique.** They are the proxy prefix and the namespace;
///     two entries claiming one id would make `/applet/<id>/`
///     ambiguous. TOML cannot enforce this for us since `[[applets]]`
///     is an array.
///   * **`user` is reserved.** See [`RESERVED_APPLET_ID`].
///
/// Nothing here looks at step ids: an applet writes no artifacts, so
/// the two namespaces cannot collide. The scaffold depends on that —
/// its `unified_index` applet sits beside its `unified_index/grid`
/// step.
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

/// The key serde is complaining about: the first backticked word in
/// its message (`unknown field \`title\`, expected one of …`).
fn complained_about(message: &str) -> Option<&str> {
    message.split('`').nth(1)
}

/// Narrow a diagnostic about an entry to the one key it is about.
///
/// `toml::Value::try_into` reports no span — the value came from a
/// tree, not from text — so all we start with is the entry's
/// `[[steps]]` header. That is never *wrong*, just coarse: it puts the
/// caret a few lines above the actual mistake, and puts the UI
/// editor's highlight there too.
///
/// The message does name the key (serde's "unknown field `title`"), and
/// the key is almost always written plainly inside the entry, so look
/// for it. The search stops at the next table header, which keeps it
/// out of a following `[steps.params]` — where an arbitrary key is
/// legal and finding one would be a lie.
///
/// Falls back to the header span whenever anything about that doesn't
/// hold, so this can improve a location and never invent one.
fn key_span(text: &str, header: std::ops::Range<usize>, key: &str) -> std::ops::Range<usize> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return header;
    }
    let mut at = header.end;
    for line in text[header.end..].split_inclusive('\n') {
        let line_start = at;
        at += line.len();
        let trimmed = line.trim_start();
        // A table header ends this entry's body. Stopping here keeps
        // the search out of a following `[steps.params]`, where an
        // arbitrary key is legal and a match would be a lie.
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

/// What the entry-level pass produced: the config that survived, the
/// specs to graph, where each surviving step sits in the file, and one
/// diagnostic per entry that did not make it.
struct Entries {
    cfg: DagConfig,
    specs: Vec<StepSpec>,
    /// step id → byte range of the `[[steps]]` header declaring it.
    /// Only for surviving steps; a step that was dropped already has a
    /// located diagnostic of its own.
    spans: BTreeMap<String, std::ops::Range<usize>>,
    diagnostics: Vec<Diagnostic>,
}

/// Deserialize every entry on its own and apply every rule that does
/// not need the graph.
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
                diagnostics: vec![d],
            };
        }
    };

    let mut diags = Vec::new();
    let mut spans: BTreeMap<String, std::ops::Range<usize>> = BTreeMap::new();

    // Each entry deserialized on its own, so one bad key costs one
    // entry. The id is read straight off the raw value rather than
    // taken from the deserialized entry, because a *rejected* entry
    // still has to be nameable — and the key that failed is usually not
    // the id.
    let mut step_candidates = Vec::with_capacity(raw.steps.len());
    for (i, spanned) in raw.steps.into_iter().enumerate() {
        let span = spanned.span();
        let value = spanned.into_inner();
        let id = value.get("id").and_then(|v| v.as_str()).map(str::to_string);
        if let Some(id) = &id {
            // First spelling wins, matching which entry `accept_steps`
            // keeps when two claim one id.
            spans.entry(id.clone()).or_insert_with(|| span.clone());
        }
        let reference = EntryRef::step(i, id);
        match value.try_into::<StepEntry>() {
            Ok(entry) => step_candidates.push(Candidate {
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

    let mut applet_candidates = Vec::with_capacity(raw.applets.len());
    for (i, spanned) in raw.applets.into_iter().enumerate() {
        let span = spanned.span();
        let value = spanned.into_inner();
        let id = value.get("id").and_then(|v| v.as_str()).map(str::to_string);
        let reference = EntryRef::applet(i, id);
        match value.try_into::<AppletEntry>() {
            Ok(entry) => applet_candidates.push(Candidate {
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

    let (accepted, step_diags) = accept_steps(step_candidates, Some(text));
    let (applets, applet_diags) = accept_applets(applet_candidates, Some(text));
    diags.extend(step_diags);
    diags.extend(applet_diags);

    let mut steps = Vec::with_capacity(accepted.len());
    let mut specs = Vec::with_capacity(accepted.len());
    for (entry, spec) in accepted {
        steps.push(entry);
        specs.push(spec);
    }

    Entries {
        cfg: DagConfig {
            data_root: raw.data_root,
            binary_dir: raw.binary_dir,
            steps,
            applets,
        },
        specs,
        spans,
        diagnostics: diags,
    }
}

/// Parse config text, keeping every entry that loads.
///
/// Stops at the file level only: malformed TOML, or a top-level key we
/// do not know, leaves nothing to salvage and comes back as a single
/// [`Severity::Fatal`] diagnostic with an empty config. Anything
/// smaller costs its own entry and nothing else.
///
/// Does not build the graph, so `inputs` are unresolved here. Callers
/// that want the whole answer want [`check_text`].
pub fn parse_graded(text: &str) -> (DagConfig, Vec<Diagnostic>) {
    let e = entries_of(text);
    (e.cfg, e.diagnostics)
}

/// Everything the loader can say about one config text.
pub struct ConfigCheck {
    /// The exact bytes checked. Diagnostics carry byte spans into it,
    /// so rendering one needs it — keeping it here means no caller has
    /// to remember to carry the two together.
    pub text: String,
    /// The entries that survived — exactly the steps in `graph`, plus
    /// the applets that loaded. A valid config: a caller may use it
    /// without looking at the diagnostics at all.
    pub cfg: DagConfig,
    /// The graph built from `cfg`, ready to run. Empty when the file is
    /// not a config.
    pub graph: Graph,
    /// One per problem, in file order — the order someone fixing them
    /// reads. Sort by [`Severity`] for worst-first; its `Ord` is blast
    /// radius.
    pub diagnostics: Vec<Diagnostic>,
}

impl ConfigCheck {
    /// Nothing loaded: the file is not a config. The one state that
    /// should stop the whole app.
    pub fn is_fatal(&self) -> bool {
        self.worst() == Some(Severity::Fatal)
    }

    /// Everything in the file loaded.
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// The largest blast radius in the file, if any.
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

    /// Every diagnostic rendered for a terminal, newline-separated.
    pub fn render(&self, path: &Path) -> String {
        self.diagnostics
            .iter()
            .map(|d| d.render(path, &self.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The whole chokepoint: config text in, the graph that will actually
/// run out, plus one diagnostic per entry that will not.
///
/// This is what every entry point should call — the `datalib-dag`
/// binary, `datalib-http`'s config load, and its `PUT /api/config`
/// validation. One function rather than a rule in each caller is
/// deliberate: the config file is the source of truth, so a rule one
/// caller enforces alone is a rule a hand-edit silently breaks.
pub fn check_text(text: &str) -> ConfigCheck {
    let mut entries = entries_of(text);
    let (graph, mut graph_diags) = Graph::build_graded(std::mem::take(&mut entries.specs));

    // Graph assembly drops more than the entry pass could see — a step
    // whose input names nothing, a ring — so the surviving *config* is
    // narrowed to what the graph kept. Leaving the entries in would
    // give `cfg` and `graph` two different answers to "what survived",
    // and every caller would have to know which one it meant.
    entries
        .cfg
        .steps
        .retain(|s| graph.by_id.contains_key(&s.id));

    // Graph diagnostics know a step id but not where it sits in the
    // file — the graph is built from specs, which carry no spans. Lend
    // them the location here, where both are in hand, rather than
    // threading the text through graph assembly.
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
    /// A config with nothing in it — what a fatal diagnostic leaves
    /// behind. Deliberately not `Default`: "empty" here is a failure
    /// state and should read as one at the call site.
    fn empty() -> Self {
        DagConfig {
            data_root: None,
            binary_dir: None,
            steps: Vec::new(),
            applets: Vec::new(),
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
                id = "slack/rendered_md"
                inputs = ["slack/raw"]
                command = "datalib-step render slack_api"
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
                id = "slack/raw"
                command = "datalib-step download slack_api"
                params.sync = {{ since = "{since}" }}
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

    #[test]
    fn command_gets_declared_fields_as_json_flags() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "slack/raw"
            command = "datalib-step download slack_api"
            params.sync = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            id = "slack/rendered_md"
            inputs = ["slack/raw"]
            command = "datalib-step render slack_api"
            params.sync = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            id = "unified_index/grid"
            inputs = ["slack/rendered_md"]
            command = "datalib-step grid_index"
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
        assert_eq!(&dl[..3], &["datalib-step", "download", "slack_api"]);
        assert_eq!(dl[3], "--params");
        let params: serde_json::Value = serde_json::from_str(&dl[4]).unwrap();
        assert_eq!(params["sync"]["channels"][0], "chat-qi");
        // No inputs declared → no --inputs. `--outputs` is still sent,
        // now derived from the id, so a step written against the old
        // contract keeps working.
        assert_eq!(&dl[5..], &["--outputs", r#"["slack/raw"]"#]);

        // TOML has no anchors, so the render step repeats the subtree —
        // and must produce byte-identical JSON for it.
        let rn = argv(1);
        assert_eq!(rn[1], "render");
        assert_eq!(rn[4], dl[4]);
        assert_eq!(
            &rn[5..],
            &[
                "--inputs",
                r#"["slack/raw"]"#,
                "--outputs",
                r#"["slack/rendered_md"]"#
            ]
        );

        // Param-less step: just inputs + outputs.
        assert_eq!(
            argv(2),
            vec![
                "datalib-step",
                "grid_index",
                "--inputs",
                r#"["slack/rendered_md"]"#,
                "--outputs",
                r#"["unified_index/grid"]"#
            ]
        );

        // Edges are the declared inputs, nothing more.
        let g = crate::Graph::build(specs).unwrap();
        assert_eq!(g.deps[g.by_id["unified_index/grid"]].len(), 1);
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
                assert_eq!(
                    &argv[..3],
                    &["sh", "-c", r#"echo "hi there" > custom/out/x.txt"#]
                );
                assert_eq!(&argv[3..], &["--outputs", r#"["custom/out"]"#]);
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
        let bare: DagConfig = toml::from_str(
            r#"steps = [{id = "slack/raw", command = "datalib-step download slack_api"}]"#,
        )
        .unwrap();
        let named: DagConfig = toml::from_str(
            r#"steps = [{id = "slack/raw", name = "Work Slack", command = "datalib-step download slack_api"}]"#,
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

    /// `unified_index` needs no reserved-name rule any more: the index
    /// steps' ids *are* those trees, so anything else claiming one is
    /// an ordinary duplicate id. What must keep working is the index
    /// steps themselves.
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

    #[test]
    fn missing_command_is_rejected_at_parse() {
        let err = toml::from_str::<DagConfig>(r#"steps = [{id = "x/out"}]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("command"), "{err}");
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
            params.sync = {since = 2026-06-15, at = 2026-06-15T10:30:00Z}
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        let StepRun::Subprocess { argv, .. } = &specs[0].run else {
            panic!("expected subprocess");
        };
        let params: serde_json::Value = serde_json::from_str(&argv[2]).unwrap();
        assert_eq!(params["sync"]["since"], "2026-06-15");
        assert_eq!(params["sync"]["at"], "2026-06-15T10:30:00Z");
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

    /// `title` was accepted and read by nothing; removing it makes a
    /// config that still carries one fail to parse. Pinned because
    /// that is what a user upgrading hits, and the message has to name
    /// the key so the fix is obvious — `deny_unknown_fields` is what
    /// makes it name the key rather than ignore it.
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
id = "slack/raw"
command = "datalib-step download slack_api"
title = "Work Slack"

[[steps]]
id = "pdfs/raw"
command = "datalib-step download pdf"

[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"
inputs = ["pdfs/raw"]

[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"
"#,
        );
        // The broken step is gone and named; everything else loaded.
        assert_eq!(ids(&check), vec!["pdfs/raw", "unified_index/grid"]);
        assert_eq!(check.cfg.applets.len(), 1, "the applet must survive");
        assert_eq!(
            sev_of(&check.diagnostics, "slack/raw"),
            Some(Severity::Rejected)
        );
        assert!(!check.is_fatal());
        assert_eq!(check.dropped(), 1);

        // …and the message names the key, which is what makes it
        // fixable without reading the schema.
        let d = check
            .diagnostics
            .iter()
            .find(|d| d.id() == Some("slack/raw"))
            .unwrap();
        assert!(d.message.contains("title"), "{}", d.message);
        assert_eq!(
            d.line,
            Some(5),
            "the `title =` line itself, not the entry header"
        );
    }

    /// Nested ids were **silently accepted** before #209: uniqueness was
    /// string equality, and a step's id is a path. Two steps writing
    /// under one tree is two writers on one `.doltlite_db`, whose
    /// working set is shared across processes — so they commit each
    /// other's in-flight rows, with no failed step and no log line.
    ///
    /// Checked both ways round, because the file can declare them in
    /// either order.
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

    /// An applet id and a step id are separate namespaces, and the
    /// scaffold depends on it: `unified_index` the applet sits beside
    /// `unified_index/grid` the step. A containment check that spanned
    /// both would reject every default config.
    #[test]
    fn the_scaffold_shape_loads_clean() {
        let check = check_text(
            r#"
[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"

[[steps]]
id = "unified_index/qmd"
command = "datalib-step qmd_index"

[[applets]]
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
        assert!(help.contains("input_path"), "{help}");

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
        assert!(check.cfg.steps[0].command.contains("first"));
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

    /// A diagnostic points at the key it is about, not at the entry it
    /// is in — which is where a reader looks and where the UI editor
    /// puts its highlight. `toml::Value::try_into` reports no span at
    /// all, so this is found rather than given, and the fallback is
    /// the entry header.
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
    /// `parse` rejects is exactly what produces a diagnostic.
    #[test]
    fn strict_parse_rejects_exactly_what_the_graded_loader_reports() {
        for text in [
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\ntitle = 1\n",
            "[[steps]]\nid = \"a\"\ncommand = \"x\"\n[[steps]]\nid = \"a\"\ncommand = \"y\"\n",
            "[[steps]]\nid = \"system\"\ncommand = \"x\"\n",
            "[[applets]]\nid = \"user\"\ncommand = \"x\"\n",
            "nope = 1\n",
        ] {
            let (_, diags) = parse_graded(text);
            assert!(!diags.is_empty(), "graded accepted {text:?}");
            assert!(parse(text).is_err(), "strict accepted {text:?}");
        }
    }
}
