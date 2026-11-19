//! The shape from before the runner treated every program alike. A step
//! was `group` + `function`; `datalib-step` read its function and its
//! group's type from the environment the runner set; and the runner gave
//! `datalib-step`'s steps the locks, `reads` and verbs it knew they
//! needed. The rewrite writes all of that into the file, as
//! `docs/dev/plans/dag_and_step_decoupling.md` §"Existing configs" lists:
//! each step gets its `id`, its whole `command`, and the `reads` and
//! `locks` the runner used to give it; the file gets a `[[programs]]`
//! entry for `datalib-step`, the `[[locks]]` its steps hold, and an
//! `[env]` table in place of `checkpoint_cadence`, `render_pages` and
//! `embed_contexts`; a group loses its `source`.
//!
//! A text edit, like `step_commands.rs`: every comment and key the file
//! holds survives, and the comments above a top-level key that moves into
//! `[env]` move with it. A step or group written as an inline table is
//! written again as one, since an inline table holds no comments.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::{bail, Context as _, Result};
use datalib_dag::config_order::entry_ranges;
use serde::Deserialize;
use toml::Spanned;

use crate::retired::{is_datalib_step, STEP_PROGRAM};

/// The verbs `datalib-step` takes.
const STEP_VERBS: &str = r#"["reset", "prepare", "compact", "collect"]"#;

const QMD_KEYWORD: &str = "qmd_keyword";
const QMD_EMBED: &str = "qmd_embed";
const INDEX: &str = "index";

/// The locks the runner used to declare in every config, which a step
/// may now hold only once the file declares them: name, slots, and the
/// comment written above the entry.
pub(crate) const RETIRED_DEFAULT_LOCKS: &[(&str, usize, &str)] = &[
    (
        QMD_KEYWORD,
        1,
        "Whatever writes qmd's keyword index: two at once can lose a document.",
    ),
    (
        QMD_EMBED,
        1,
        "Whatever embeds into qmd's index: two at once race on its vectors.",
    ),
    (INDEX, 2, "The fan-ins that read every source."),
];

/// The top-level keys that become `[env]` variables.
const CHECKPOINT_CADENCE: &str = "checkpoint_cadence";
const RENDER_PAGES: &str = "render_pages";
const EMBED_CONTEXTS: &str = "embed_contexts";

/// The functions of `datalib-step` that read their inputs' files off disk
/// rather than at a pinned commit, `None` matching any group type.
const READS_FILES: &[(Option<&str>, &str)] = &[
    (None, "keyword_index"),
    (None, "embed"),
    (None, "embedding_map"),
    (Some("perseus"), "render_markdown"),
];

const DIFF_TYPE: &str = "diff";
const INGEST: &str = "ingest";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    groups: Vec<Spanned<GroupView>>,
    #[serde(default)]
    steps: Vec<Spanned<StepView>>,
    #[serde(default)]
    programs: Vec<ProgramView>,
    #[serde(default)]
    locks: Vec<LockView>,
    #[serde(default)]
    env: Option<Spanned<toml::Table>>,
    #[serde(default)]
    checkpoint_cadence: Option<Spanned<toml::Value>>,
    #[serde(default)]
    render_pages: Option<Spanned<toml::Value>>,
    #[serde(default)]
    embed_contexts: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize)]
struct GroupView {
    id: String,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    source: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    id: Option<Spanned<String>>,
    #[serde(default)]
    group: Option<Spanned<String>>,
    #[serde(default)]
    function: Option<Spanned<String>>,
    #[serde(default)]
    command: Option<Spanned<String>>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    reads: Option<toml::Value>,
    #[serde(default)]
    locks: Option<toml::Value>,
}

#[derive(Deserialize)]
struct ProgramView {
    command: String,
}

#[derive(Deserialize)]
struct LockView {
    name: String,
}

impl StepView {
    fn name(&self) -> String {
        match (&self.id, &self.group, &self.function) {
            (Some(id), _, _) => id.get_ref().clone(),
            (None, Some(g), Some(f)) => format!("{}/{}", g.get_ref(), f.get_ref()),
            _ => "(unnamed)".to_string(),
        }
    }

    /// Whether this step is in the old shape.
    fn is_retired(&self) -> bool {
        self.group.is_some() || self.function.is_some() || self.runs_step_without_run()
    }

    /// It runs `datalib-step` with no `run` after it, or names no program.
    fn runs_step_without_run(&self) -> bool {
        let Some(command) = &self.command else {
            return true;
        };
        let mut words = command.get_ref().split_whitespace();
        words.next().is_some_and(is_datalib_step) && words.next() != Some("run")
    }

    /// The names of the locks it writes.
    fn written_locks(&self) -> Vec<String> {
        match &self.locks {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(toml::Value::Table(t)) => t.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

/// What the rewrite does to one step.
struct StepPlan {
    /// `<group>/<function>`, when it was written as the two.
    id: Option<String>,
    /// Its new `command`, when that changes.
    command: Option<String>,
    /// The `datalib-step` program word it runs, as written.
    step_program: Option<String>,
    reads: Option<&'static str>,
    lock: Option<&'static str>,
}

fn plan_step(s: &StepView, groups: &BTreeMap<&str, &GroupView>) -> Result<StepPlan> {
    let name = s.name();
    let (id, group) = match (&s.id, &s.group, &s.function) {
        (Some(_), None, None) => (None, None),
        (None, Some(g), Some(f)) => (
            Some(format!("{}/{}", g.get_ref(), f.get_ref())),
            Some(g.get_ref().as_str()),
        ),
        (Some(_), _, _) => bail!(
            "step {name} has `id` and also `group` or `function`; the old loader refused it \
             too. Keep `id` or the other two, then run this again"
        ),
        _ => bail!("step {name} needs both `group` and `function`, as the old loader did"),
    };
    let function = s.function.as_ref().map(|f| f.get_ref().as_str());
    let group_entry = group.and_then(|g| groups.get(g).copied());
    let group_type = group_entry.and_then(|g| g.r#type.as_deref());

    let words: Vec<&str> = s
        .command
        .as_ref()
        .map(|c| c.get_ref().split_whitespace().collect())
        .unwrap_or_default();
    let step_program = match words.first() {
        None => Some(STEP_PROGRAM),
        Some(p) if is_datalib_step(p) => Some(*p),
        Some(_) => None,
    };
    let mut command = None;
    if let Some(program) = step_program {
        match words.get(1) {
            Some(&"run") => {}
            Some(word) => bail!(
                "step {name} runs `{program} {word}`, a shape from before steps were grouped; \
                 `datalib-migrate-config` rewrites it"
            ),
            None => {
                let Some(function) = function else {
                    bail!(
                        "step {name} runs `{program}` but has no `function` to tell it what \
                         to do; write its `command` as `{program} run <function>`"
                    );
                };
                let mut line = format!("{program} run {function}");
                if let Some(t) = group_type {
                    line.push_str(&format!(" {t}"));
                }
                if group_type == Some(DIFF_TYPE) {
                    let source = group_entry
                        .and_then(|g| g.source.as_ref())
                        .map(|s| s.get_ref().as_str());
                    let source_type = source
                        .and_then(|s| groups.get(s))
                        .and_then(|g| g.r#type.as_deref());
                    let Some(source_type) = source_type else {
                        bail!(
                            "step {name} is under the diff group {:?}, whose `source` names no \
                             group with a `type`",
                            group.unwrap_or_default()
                        );
                    };
                    line.push_str(&format!(" --source-type {source_type}"));
                }
                command = Some(line);
            }
        }
    }

    let has_inputs = !s.inputs.is_empty();
    let is_step_program = step_program.is_some();
    // A step already in the new shape (`datalib-step run …`) under a
    // `group`/`function` pair keeps its words; its function still says
    // what the runner gave it.
    let reads = match (&s.reads, is_step_program, function) {
        (Some(_), _, _) => None,
        (None, true, Some(f)) if f != INGEST => Some(
            if READS_FILES
                .iter()
                .any(|&(t, rf)| rf == f && (t.is_none() || t == group_type))
            {
                "files"
            } else {
                "pinned"
            },
        ),
        (None, true, _) => None,
        (None, false, _) => has_inputs.then_some("pinned"),
    };
    let lock = match (&s.locks, is_step_program, function) {
        (Some(_), _, _) => None,
        (None, true, Some("qmd_aggregator" | "keyword_index")) => Some(QMD_KEYWORD),
        (None, true, Some("embed")) => Some(QMD_EMBED),
        (None, true, Some(f)) => (group_type.is_none() && f != INGEST).then_some(INDEX),
        (None, true, None) => None,
        (None, false, _) => (has_inputs && group_type.is_none()).then_some(INDEX),
    };
    Ok(StepPlan {
        id,
        command,
        step_program: step_program.map(str::to_string),
        reads,
        lock,
    })
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(view.steps.iter().any(|s| s.get_ref().is_retired())
        || view.groups.iter().any(|g| g.get_ref().source.is_some())
        || view.checkpoint_cadence.is_some()
        || view.render_pages.is_some()
        || view.embed_contexts.is_some())
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Just past the newline that ends the line `at` is on.
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}

/// Where `key = ` starts, when the value at `span` is the only thing on
/// its line but a trailing comment: a key of a `[[…]]` table, not of an
/// inline one.
fn key_on_its_own_line(text: &str, key: &str, span: &Range<usize>) -> Option<usize> {
    let start = line_start(text, span.start);
    let lead = &text[start..span.start];
    let indent = lead.len() - lead.trim_start().len();
    let rest = lead.trim_start().strip_prefix(key)?.trim_start();
    if rest.strip_prefix('=')?.trim().is_empty() {
        let after = text[span.end..line_end(text, span.end)].trim();
        (after.is_empty() || after.starts_with('#')).then_some(start + indent)
    } else {
        None
    }
}

/// The edit that takes `key = value` off its line: the whole line, or
/// only the key and value when a comment follows them.
fn remove_key(text: &str, key_start: usize, span: &Range<usize>) -> (Range<usize>, String) {
    let end = line_end(text, span.end);
    if text[span.end..end].trim().is_empty() {
        (line_start(text, key_start)..end, String::new())
    } else {
        let ws = text[span.end..].len() - text[span.end..].trim_start_matches([' ', '\t']).len();
        (key_start..span.end + ws, String::new())
    }
}

/// The insertion of `lines` just past the line `at` is on.
fn insert_after_line(text: &str, at: usize, lines: &str) -> (Range<usize>, String) {
    let end = line_end(text, at);
    let lead = if end == text.len() && !text.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    (end..end, format!("{lead}{lines}"))
}

fn plan_lines(plan: &StepPlan) -> String {
    let mut lines = String::new();
    if let Some(reads) = plan.reads {
        lines.push_str(&format!("reads = {}\n", quote(reads)));
    }
    if let Some(lock) = plan.lock {
        lines.push_str(&format!("locks = [{}]\n", quote(lock)));
    }
    lines
}

/// The edits to one `[[steps]]` table, or `None` when it is an inline
/// table, which is written again whole.
fn step_edits(text: &str, s: &StepView, plan: &StepPlan) -> Option<Vec<(Range<usize>, String)>> {
    let own = |key: &str, v: &Option<Spanned<String>>| -> Option<Option<(usize, Range<usize>)>> {
        match v {
            None => Some(None),
            Some(v) => key_on_its_own_line(text, key, &v.span()).map(|k| Some((k, v.span()))),
        }
    };
    let group = own("group", &s.group)?;
    let function = own("function", &s.function)?;
    let command = own("command", &s.command)?;

    let mut edits = Vec::new();
    if let (Some((key, span)), Some(new_id)) = (&group, &plan.id) {
        edits.push((*key..span.end, format!("id = {}", quote(new_id))));
    }
    let lines = plan_lines(plan);
    match (&command, &function) {
        (None, Some((key, span))) => {
            let new = plan.command.as_deref().unwrap_or(STEP_PROGRAM);
            edits.push((*key..span.end, format!("command = {}", quote(new))));
            if !lines.is_empty() {
                edits.push(insert_after_line(text, span.end, &lines));
            }
        }
        (Some((_, cspan)), function) => {
            if let Some((key, span)) = function {
                edits.push(remove_key(text, *key, span));
            }
            if let Some(new) = &plan.command {
                edits.push((cspan.clone(), quote(new)));
            }
            if !lines.is_empty() {
                edits.push(insert_after_line(text, cspan.end, &lines));
            }
        }
        // No command and no function: `plan_step` refused it already.
        (None, None) => {}
    }
    Some(edits)
}

/// An inline step, written again with its new keys.
fn inline_step(table: &toml::Table, plan: &StepPlan) -> String {
    let mut t = table.clone();
    t.remove("group");
    t.remove("function");
    if let Some(id) = &plan.id {
        t.insert("id".into(), toml::Value::String(id.clone()));
    }
    if let Some(command) = &plan.command {
        t.insert("command".into(), toml::Value::String(command.clone()));
    }
    if let Some(reads) = plan.reads {
        t.insert("reads".into(), toml::Value::String(reads.into()));
    }
    if let Some(lock) = plan.lock {
        t.insert(
            "locks".into(),
            toml::Value::Array(vec![toml::Value::String(lock.into())]),
        );
    }
    toml::Value::Table(t).to_string()
}

/// The raw tables of the entries, for the inline ones.
#[derive(Deserialize)]
struct RawView {
    #[serde(default)]
    groups: Vec<Spanned<toml::Table>>,
    #[serde(default)]
    steps: Vec<Spanned<toml::Table>>,
}

fn is_inline(text: &str, span: &Range<usize>) -> bool {
    text[span.start..].starts_with('{')
}

/// One top-level key that becomes an `[env]` variable: the variable, its
/// value, the comments that described it, and where it was.
struct Moved {
    var: &'static str,
    value: String,
    comments: Vec<String>,
    gone: Range<usize>,
}

fn number(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

fn env_value(key: &str, v: &toml::Value) -> Result<(&'static str, String)> {
    let bad = || anyhow::anyhow!("`{key}` is {v}, which this rewrite cannot read");
    match key {
        CHECKPOINT_CADENCE => {
            let t = v.as_table().ok_or_else(bad)?;
            if t.keys().any(|k| k != "at_most_every_secs") {
                return Err(bad());
            }
            let secs = t
                .get("at_most_every_secs")
                .and_then(number)
                .ok_or_else(bad)?;
            Ok((
                datalib_env::DATALIB_STEP_CHECKPOINT_CADENCE,
                secs.to_string(),
            ))
        }
        RENDER_PAGES => {
            let t = v.as_table().ok_or_else(bad)?;
            if t.keys().any(|k| k != "buckets" && k != "blob_mb") {
                return Err(bad());
            }
            let side = |k: &str| -> Result<String> {
                match t.get(k) {
                    None => Ok(String::new()),
                    Some(n) => Ok(n.as_integer().ok_or_else(bad)?.to_string()),
                }
            };
            Ok((
                datalib_env::DATALIB_STEP_RENDER_PAGES,
                format!("{},{}", side("buckets")?, side("blob_mb")?),
            ))
        }
        _ => Ok((
            datalib_env::DATALIB_STEP_EMBED_CONTEXTS,
            v.as_integer().ok_or_else(bad)?.to_string(),
        )),
    }
}

fn comment_lines(block: &str) -> Vec<String> {
    block
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Whether this cut of the file is the standard table `[name]`.
fn is_table(unit: &str, name: &str) -> bool {
    unit.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .and_then(|l| l.strip_prefix('['))
        .and_then(|l| l.split(']').next())
        .is_some_and(|h| h.trim() == name)
}

fn moved_key(text: &str, key: &'static str, v: &Spanned<toml::Value>) -> Result<Moved> {
    let (var, value) = env_value(key, v.get_ref())?;
    if let Some(unit) = entry_ranges(text)
        .into_iter()
        .find(|r| is_table(&text[r.clone()], key))
    {
        return Ok(Moved {
            var,
            value,
            comments: comment_lines(&text[unit.clone()]),
            gone: unit,
        });
    }
    let span = v.span();
    let Some(key_start) = key_on_its_own_line(text, key, &span) else {
        bail!("`{key}` is written in a way this rewrite cannot move; move it into `[env]` by hand");
    };
    let mut start = line_start(text, key_start);
    // The comment lines right above it describe it.
    while start > 0 {
        let above = line_start(text, start - 1);
        if !text[above..start].trim_start().starts_with('#') {
            break;
        }
        start = above;
    }
    let mut end = line_end(text, span.end);
    // Between two blank lines, take one of them too.
    if (start == 0 || text[..start].ends_with("\n\n")) && text[end..].starts_with('\n') {
        end += 1;
    }
    Ok(Moved {
        var,
        value,
        comments: comment_lines(&text[start..key_start]),
        gone: start..end,
    })
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let raw: RawView = toml::from_str(text).context("parse the config")?;
    let groups: BTreeMap<&str, &GroupView> = view
        .groups
        .iter()
        .map(|g| (g.get_ref().id.as_str(), g.get_ref()))
        .collect();

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut step_programs: BTreeSet<String> = BTreeSet::new();
    let mut held: BTreeSet<String> = BTreeSet::new();
    for (s, raw_step) in view.steps.iter().zip(&raw.steps) {
        let step = s.get_ref();
        held.extend(step.written_locks());
        let plan = plan_step(step, &groups)?;
        step_programs.extend(plan.step_program.clone());
        held.extend(plan.lock.map(str::to_string));
        if is_inline(text, &raw_step.span()) {
            edits.push((raw_step.span(), inline_step(raw_step.get_ref(), &plan)));
            continue;
        }
        match step_edits(text, step, &plan) {
            Some(e) => edits.extend(e),
            None => bail!(
                "step {} is written in a way this rewrite cannot edit; write it with one key \
                 per line",
                step.name()
            ),
        }
    }

    for (g, raw_group) in view.groups.iter().zip(&raw.groups) {
        let Some(source) = &g.get_ref().source else {
            continue;
        };
        if is_inline(text, &raw_group.span()) {
            let mut t = raw_group.get_ref().clone();
            t.remove("source");
            edits.push((raw_group.span(), toml::Value::Table(t).to_string()));
        } else if let Some(key) = key_on_its_own_line(text, "source", &source.span()) {
            edits.push(remove_key(text, key, &source.span()));
        } else {
            bail!(
                "group {}'s `source` is written in a way this rewrite cannot remove",
                g.get_ref().id
            );
        }
    }

    let mut moved = Vec::new();
    for (key, v) in [
        (CHECKPOINT_CADENCE, &view.checkpoint_cadence),
        (RENDER_PAGES, &view.render_pages),
        (EMBED_CONTEXTS, &view.embed_contexts),
    ] {
        if let Some(v) = v {
            moved.push(moved_key(text, key, v)?);
        }
    }
    let written_env: BTreeSet<&str> = view
        .env
        .as_ref()
        .map(|e| e.get_ref().keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut env_lines = String::new();
    for m in &moved {
        edits.push((m.gone.clone(), String::new()));
        if written_env.contains(m.var) {
            continue;
        }
        for c in &m.comments {
            env_lines.push_str(c);
            env_lines.push('\n');
        }
        env_lines.push_str(&format!("{} = {}\n", m.var, quote(&m.value)));
    }
    let mut tail = Vec::new();
    if !env_lines.is_empty() {
        match &view.env {
            None => tail.push(format!("[env]\n{env_lines}")),
            Some(env) => {
                let Some(unit) = entry_ranges(text)
                    .into_iter()
                    .find(|r| is_table(&text[r.clone()], "env"))
                else {
                    bail!(
                        "`env` is written in a way this rewrite cannot add to: {}",
                        &text[env.span()]
                    );
                };
                let at = unit.start + text[unit.clone()].trim_end().len();
                edits.push((at..at, format!("\n{}", env_lines.trim_end())));
            }
        }
    }

    let declared_programs: BTreeSet<&str> =
        view.programs.iter().map(|p| p.command.as_str()).collect();
    for program in step_programs
        .iter()
        .filter(|p| !declared_programs.contains(p.as_str()))
    {
        tail.push(format!(
            "# The verbs `datalib-step` takes (docs/dev/step_protocol.md § Verbs).\n\
             [[programs]]\ncommand = {}\nverbs = {STEP_VERBS}\n",
            quote(program)
        ));
    }
    let declared_locks: BTreeSet<&str> = view.locks.iter().map(|l| l.name.as_str()).collect();
    for (name, slots, why) in RETIRED_DEFAULT_LOCKS {
        if held.contains(*name) && !declared_locks.contains(name) {
            tail.push(format!(
                "# {why}\n[[locks]]\nname = {}\nslots = {slots}\n",
                quote(name)
            ));
        }
    }

    // Spans index the original text, so editing from the end leaves every
    // earlier span where it was; at one place, the longer edit goes first
    // so an insertion lands in front of it.
    edits.sort_by_key(|(r, _)| (std::cmp::Reverse(r.start), std::cmp::Reverse(r.end)));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
    }
    if !tail.is_empty() {
        out = format!("{}\n", out.trim_end());
        for block in tail {
            out.push('\n');
            out.push_str(&block);
        }
    }
    name_backup_secrets(&out)
}

/// The variable each source type's ingest reads its backup secret from
/// when its params name none, and the params key that names it.
const BACKUP_SECRETS: &[(&str, &str, &str)] = &[
    ("whatsapp", "key_env_var", "WHATSAPP_BACKUP_DECRYPTION_KEY"),
    ("signal", "aep_env_var", "SIGNAL_BACKUP_PASSPHRASE"),
];

/// The runner used to hand `datalib-step` these two secrets by name; now
/// a step gets a variable of the runner's environment only when its
/// params name it (`docs/dev/step_protocol.md` § "What your command
/// receives"). So a WhatsApp or Signal ingest that relied on the default
/// gets the default written into its `[steps.params.backup]`, and keeps
/// working when `datalib-dag` runs it from a terminal. A step whose
/// params are written another way is left alone: the app hands both
/// variables to every step itself.
fn name_backup_secrets(text: &str) -> Result<String> {
    let doc: toml::Value = toml::from_str(text).context("parse the rewritten config")?;
    let mut out = text.to_string();
    let steps = doc
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for step in &steps {
        let (Some(id), Some(command)) = (
            step.get("id").and_then(|v| v.as_str()),
            step.get("command").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let argv: Vec<&str> = command.split_whitespace().collect();
        let Some((key, var)) = (match argv.as_slice() {
            [program, "run", ingest, source_type, ..]
                if is_datalib_step(program) && *ingest == INGEST =>
            {
                BACKUP_SECRETS
                    .iter()
                    .find(|(t, _, _)| t == source_type)
                    .map(|&(_, key, var)| (key, var))
            }
            _ => None,
        }) else {
            continue;
        };
        let backup = step.get("params").and_then(|p| p.get("backup"));
        if backup.is_none_or(|b| b.get(key).is_some()) {
            continue;
        }
        let Some(at) = backup_header_end(&out, id) else {
            continue;
        };
        out.insert_str(at, &format!("\n{key} = {}", quote(var)));
    }
    Ok(out)
}

/// The end of the `[steps.params.backup]` header line that belongs to
/// the step with `id`, when it is written as one.
fn backup_header_end(text: &str, id: &str) -> Option<usize> {
    let id_line = format!("id = {}", quote(id));
    let start = text
        .match_indices(&id_line)
        .map(|(i, _)| i)
        .find(|&i| text[line_start(text, i)..i].trim().is_empty())?;
    let mut at = line_end(text, start);
    while at < text.len() {
        let end = line_end(text, at);
        let line = text[at..end].trim();
        if line.starts_with("[[") {
            return None;
        }
        if line == "[steps.params.backup]" {
            return Some(at + text[at..end].trim_end_matches(['\n', '\r']).len());
        }
        at = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"# the Enterprise's library
data_root = "/deck8"

# one context is plenty on a mac
embed_contexts = 2

[run_history]
max_runs = 7

# smaller pages for a small machine
[render_pages]
buckets = 100

[[groups]]
id = "picard"
type = "slack"

# the captain's log
[[steps]]
group = "picard"
function = "ingest"  # every channel
command = "datalib-step"
[steps.params.api]
workspace = "enterprise"

[[steps]]
group = "picard"
function = "render_markdown"
inputs = ["picard/ingest"]

[[steps]]
group = "picard"
function = "keyword_index"
command = "datalib-step"
inputs = ["picard/render_markdown"]

[[steps]]
group = "picard"
function = "embed"
command = "datalib-step"
inputs = ["picard/keyword_index"]

[[groups]]
id = "picard-diff"
type = "diff"
source = "picard"

[[steps]]
group = "picard-diff"
function = "render_markdown"
command = "datalib-step"
inputs = ["picard/ingest"]
[steps.params.diff]
from = "aaaa"
to = "bbbb"

[[groups]]
id = "data"
type = "perseus"

[[steps]]
group = "data"
function = "ingest"
command = "/opt/datalib/datalib-step"

[[steps]]
group = "data"
function = "render_markdown"
command = "/opt/datalib/datalib-step"
inputs = ["data/ingest"]

[[groups]]
id = "unified_document_index"

[[steps]]
group = "unified_document_index"
function = "grid_index"
command = "datalib-step"
inputs = []

[[steps]]
group = "unified_document_index"
function = "qmd_aggregator"
command = "datalib-step"
inputs = ["picard/keyword_index", "picard/embed"]

[[steps]]
group = "unified_document_index"
function = "embedding_map"
command = "datalib-step"
inputs = ["unified_document_index/qmd_aggregator"]

[[steps]]
group = "picard"
function = "export"
command = "my-exporter --csv"
inputs = ["picard/render_markdown"]

[[steps]]
id = "reports/weekly"
command = "my-reporter"
inputs = ["picard/render_markdown"]

[[steps]]
id = "reports/fetch"
command = "my-fetcher"

[[steps]]
id = "reports/mine"
command = "my-reporter"
inputs = ["picard/render_markdown"]
reads = "files"
locks = ["qmd_embed"]

[checkpoint_cadence]
at_most_every_secs = 2.5
"#;

    /// Each step's table in `text`, by id, as the loader reads it.
    fn step(text: &str, id: &str) -> toml::Table {
        let check = datalib_dag::config::check_text(text);
        assert!(
            check.nothing_dropped(),
            "{}\n{text}",
            check.render("config.toml".as_ref())
        );
        assert!(check.cfg.steps.iter().any(|s| s.id == id), "no {id}");
        let all: toml::Table = toml::from_str(text).unwrap();
        all["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_table().unwrap().clone())
            .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id))
            .unwrap_or_else(|| panic!("no step {id}:\n{text}"))
    }

    fn key<'a>(t: &'a toml::Table, k: &str) -> Option<&'a str> {
        t.get(k).and_then(|v| v.as_str())
    }

    fn locks(t: &toml::Table) -> Option<Vec<String>> {
        t.get("locks").map(|v| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap().to_string())
                .collect()
        })
    }

    fn after() -> String {
        let out = rewrite(BEFORE).unwrap();
        assert!(!is_retired(&out).unwrap(), "{out}");
        out
    }

    /// A WhatsApp or Signal ingest that relied on the runner handing
    /// `datalib-step` its backup secret gets the variable named in its
    /// params, since the runner now hands a step only what its params
    /// name; one that names its own keeps it.
    #[test]
    fn a_backup_ingest_names_the_variable_it_used_to_be_handed() {
        let before = r#"[[groups]]
id = "phone"
type = "whatsapp"

[[groups]]
id = "sig"
type = "signal"

[[groups]]
id = "work-phone"
type = "whatsapp"

[[steps]]
group = "phone"
function = "ingest"

[steps.params.backup]
path = "/backups/phone"

[[steps]]
group = "sig"
function = "ingest"

[steps.params.backup]
path = "/backups/sig"

[[steps]]
group = "work-phone"
function = "ingest"

[steps.params.backup]
path = "/backups/work"
key_env_var = "WORK_PHONE_KEY"
"#;
        let out = rewrite(before).unwrap();
        let named = |id: &str, k: &str| {
            step(&out, id)["params"]["backup"]
                .get(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        assert_eq!(
            named("phone/ingest", "key_env_var").as_deref(),
            Some("WHATSAPP_BACKUP_DECRYPTION_KEY"),
            "{out}"
        );
        assert_eq!(
            named("sig/ingest", "aep_env_var").as_deref(),
            Some("SIGNAL_BACKUP_PASSPHRASE"),
            "{out}"
        );
        assert_eq!(
            named("work-phone/ingest", "key_env_var").as_deref(),
            Some("WORK_PHONE_KEY"),
            "{out}"
        );
        assert_eq!(rewrite(&out).unwrap(), out, "a second pass changes nothing");
    }

    #[test]
    fn group_and_function_become_the_id() {
        let out = after();
        assert!(!out.contains("group = "), "{out}");
        assert!(!out.contains("function = "), "{out}");
        assert!(
            out.contains("[[steps]]\nid = \"picard/render_markdown\"\n"),
            "{out}"
        );
        // A custom step under a group keeps its command and gets its id.
        let export = step(&out, "picard/export");
        assert_eq!(key(&export, "command"), Some("my-exporter --csv"));
    }

    #[test]
    fn a_datalib_step_command_names_its_function_and_its_groups_type() {
        let out = after();
        let command = |id| key(&step(&out, id), "command").map(str::to_string);
        assert_eq!(
            command("picard/ingest").as_deref(),
            Some("datalib-step run ingest slack")
        );
        // Written where `function` was, for a step that named no program.
        assert!(
            out.contains(
                "id = \"picard/render_markdown\"\ncommand = \"datalib-step run render_markdown slack\"\n"
            ),
            "{out}"
        );
        assert_eq!(
            command("unified_document_index/grid_index").as_deref(),
            Some("datalib-step run grid_index")
        );
    }

    #[test]
    fn the_program_word_is_kept_as_written() {
        let out = after();
        assert_eq!(
            key(&step(&out, "data/ingest"), "command"),
            Some("/opt/datalib/datalib-step run ingest perseus")
        );
    }

    #[test]
    fn a_diff_groups_step_names_its_sources_type_and_the_group_loses_source() {
        let out = after();
        assert_eq!(
            key(&step(&out, "picard-diff/render_markdown"), "command"),
            Some("datalib-step run render_markdown diff --source-type slack")
        );
        assert!(!out.contains("source = "), "{out}");
        let check = datalib_dag::config::check_text(&out);
        assert!(check.cfg.groups.iter().any(|g| g.id == "picard-diff"));
    }

    #[test]
    fn reads_is_written_as_the_runner_used_to_work_it_out() {
        let out = after();
        let reads = |id| key(&step(&out, id), "reads").map(str::to_string);
        for id in [
            "picard/keyword_index",
            "picard/embed",
            "unified_document_index/embedding_map",
            "data/render_markdown",
        ] {
            assert_eq!(reads(id).as_deref(), Some("files"), "{id}");
        }
        for id in [
            "picard/render_markdown",
            "picard-diff/render_markdown",
            "unified_document_index/qmd_aggregator",
            // A fan-in with no inputs yet gets it too, for when it has some.
            "unified_document_index/grid_index",
            "picard/export",
            "reports/weekly",
        ] {
            assert_eq!(reads(id).as_deref(), Some("pinned"), "{id}");
        }
        for id in ["picard/ingest", "data/ingest", "reports/fetch"] {
            assert_eq!(reads(id), None, "{id}");
        }
        assert_eq!(reads("reports/mine").as_deref(), Some("files"));
    }

    #[test]
    fn locks_are_written_as_the_runner_used_to_work_them_out() {
        let out = after();
        let held = |id| locks(&step(&out, id));
        let one = |l: &str| Some(vec![l.to_string()]);
        assert_eq!(held("picard/keyword_index"), one(QMD_KEYWORD));
        assert_eq!(
            held("unified_document_index/qmd_aggregator"),
            one(QMD_KEYWORD)
        );
        assert_eq!(held("picard/embed"), one(QMD_EMBED));
        assert_eq!(held("unified_document_index/grid_index"), one(INDEX));
        assert_eq!(held("unified_document_index/embedding_map"), one(INDEX));
        assert_eq!(held("reports/weekly"), one(INDEX));
        assert_eq!(held("reports/mine"), one(QMD_EMBED));
        for id in [
            "picard/ingest",
            "picard/render_markdown",
            "picard-diff/render_markdown",
            "picard/export",
            "reports/fetch",
        ] {
            assert_eq!(held(id), None, "{id}");
        }
    }

    #[test]
    fn the_file_gets_its_programs_locks_and_env() {
        let out = after();
        let check = datalib_dag::config::check_text(&out);
        let programs: Vec<(&str, Vec<&str>)> = check
            .cfg
            .programs
            .iter()
            .map(|p| {
                (
                    p.command.as_str(),
                    p.verbs.iter().map(|v| v.as_str()).collect(),
                )
            })
            .collect();
        let verbs = vec!["reset", "prepare", "compact", "collect"];
        assert_eq!(
            programs,
            [
                ("/opt/datalib/datalib-step", verbs.clone()),
                ("datalib-step", verbs)
            ]
        );
        let declared: Vec<(&str, usize)> = check
            .cfg
            .locks
            .iter()
            .map(|l| (l.name.as_str(), l.slots))
            .collect();
        assert_eq!(declared, [(QMD_KEYWORD, 1), (QMD_EMBED, 1), (INDEX, 2)]);
        let env: Vec<(&str, &str)> = check
            .cfg
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            env,
            [
                ("DATALIB_STEP_CHECKPOINT_CADENCE", "2.5"),
                ("DATALIB_STEP_EMBED_CONTEXTS", "2"),
                ("DATALIB_STEP_RENDER_PAGES", "100,"),
            ]
        );
        for gone in ["embed_contexts", "[render_pages]", "[checkpoint_cadence]"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
    }

    /// A lock the file already declares, or no step holds, is not added.
    #[test]
    fn a_lock_is_declared_once_and_only_when_held() {
        let text =
            "[[groups]]\nid = \"u\"\n\n[[steps]]\ngroup = \"u\"\nfunction = \"grid_index\"\n\
                    inputs = []\n\n[[locks]]\nname = \"index\"\nslots = 3\n";
        let out = rewrite(text).unwrap();
        let check = datalib_dag::config::check_text(&out);
        let declared: Vec<(&str, usize)> = check
            .cfg
            .locks
            .iter()
            .map(|l| (l.name.as_str(), l.slots))
            .collect();
        assert_eq!(declared, [(INDEX, 3)], "{out}");
    }

    #[test]
    fn every_comment_and_key_survives() {
        let out = after();
        for kept in [
            "# the Enterprise's library\n",
            "data_root = \"/deck8\"\n",
            "[run_history]\nmax_runs = 7\n",
            "# the captain's log\n[[steps]]\n",
            "id = \"picard/ingest\"\n# every channel\ncommand = ",
            "[steps.params.api]\nworkspace = \"enterprise\"\n",
            "[steps.params.diff]\nfrom = \"aaaa\"\nto = \"bbbb\"\n",
        ] {
            assert!(out.contains(kept), "{kept:?} went:\n{out}");
        }
        // The comments above a key that moved into `[env]` move with it.
        assert!(
            out.contains(
                "[env]\nDATALIB_STEP_CHECKPOINT_CADENCE = \"2.5\"\n\
                 # smaller pages for a small machine\nDATALIB_STEP_RENDER_PAGES = \"100,\"\n\
                 # one context is plenty on a mac\nDATALIB_STEP_EMBED_CONTEXTS = \"2\"\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("data_root = \"/deck8\"\n\n[run_history]"),
            "{out}"
        );
    }

    #[test]
    fn a_rewritten_config_is_current() {
        let out = after();
        assert!(!is_retired(&out).unwrap());
        let err = crate::detect(&out).unwrap_err().to_string();
        assert!(err.contains("already"), "{err}");
        // An earlier rewrite sees this config first: some of its steps
        // name no program.
        assert!(crate::detect(BEFORE).is_ok());
        assert_eq!(crate::convert(BEFORE).unwrap(), out);
        assert_eq!(
            crate::upgrade_explicit_steps(BEFORE).unwrap(),
            Some(out.clone())
        );
        assert_eq!(crate::upgrade_explicit_steps(&out).unwrap(), None);
    }

    /// Each of the shapes that make a config old, alone.
    #[test]
    fn each_old_shape_is_seen() {
        for old in [
            "[[steps]]\ngroup = \"g\"\nfunction = \"ingest\"\ncommand = \"datalib-step\"\n",
            "[[steps]]\nid = \"g/ingest\"\ncommand = \"datalib-step\"\n",
            "[[steps]]\nid = \"g/ingest\"\n",
            "embed_contexts = 1\n",
            "[render_pages]\nblob_mb = 5\n",
            "[checkpoint_cadence]\nat_most_every_secs = 1\n",
            "[[groups]]\nid = \"d\"\ntype = \"diff\"\nsource = \"g\"\n",
        ] {
            assert!(is_retired(old).unwrap(), "{old}");
        }
        for current in [
            "[[steps]]\nid = \"g/ingest\"\ncommand = \"datalib-step run ingest slack\"\n",
            "[[steps]]\nid = \"g/x\"\ncommand = \"my-tool\"\n",
            "[env]\nDATALIB_STEP_EMBED_CONTEXTS = \"1\"\n",
        ] {
            assert!(!is_retired(current).unwrap(), "{current}");
        }
    }

    /// A step written as an inline table is written again as one: TOML
    /// keeps no comments inside it to lose.
    #[test]
    fn an_inline_step_is_written_again_inline() {
        let text = "[[groups]]\nid = \"g\"\ntype = \"slack\"\n\n\
                    [[groups]]\nid = \"u\"\n\n\
                    steps = [{ group = \"g\", function = \"ingest\" }, \
                    { group = \"u\", function = \"grid_index\", inputs = [\"g/ingest\"] }]\n";
        // `steps` after a `[[groups]]` header would belong to it; put it first.
        let (groups, steps) = text.split_once("steps = ").unwrap();
        let text = format!("steps = {steps}\n{groups}");
        let out = rewrite(&text).unwrap();
        assert!(!is_retired(&out).unwrap(), "{out}");
        assert!(out.starts_with("steps = [{ "), "{out}");
        let ingest = step(&out, "g/ingest");
        assert_eq!(
            key(&ingest, "command"),
            Some("datalib-step run ingest slack")
        );
        let grid = step(&out, "u/grid_index");
        assert_eq!(key(&grid, "reads"), Some("pinned"));
        assert_eq!(locks(&grid), Some(vec![INDEX.to_string()]));
    }

    #[test]
    fn a_retired_subcommand_is_left_for_the_full_rewrite() {
        let err =
            rewrite("[[steps]]\nid = \"s/raw\"\ncommand = \"datalib-step download slack_api\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("datalib-migrate-config"), "{err}");
    }

    /// What the runner gave a step, by the rules it had at the commit
    /// before this rewrite (`git show <that commit>:<path>`).
    mod old_runner {
        use super::super::*;

        /// `UNPINNED_BUILTINS`, `datalib/backend/dag/src/config.rs:1939-1944`.
        const UNPINNED: &[(Option<&str>, &str)] = &[
            (None, "keyword_index"),
            (None, "embed"),
            (None, "embedding_map"),
            (Some("perseus"), "render_markdown"),
        ];
        /// `BUILTIN_LOCKS`, `datalib/backend/dag/src/config.rs:1949-1953`.
        const LOCKS: &[(&str, &str)] = &[
            ("qmd_aggregator", "qmd_keyword"),
            ("keyword_index", "qmd_keyword"),
            ("embed", "qmd_embed"),
        ];

        #[derive(Deserialize)]
        pub struct Old {
            #[serde(default)]
            pub groups: Vec<OldGroup>,
            #[serde(default)]
            pub steps: Vec<OldStep>,
        }
        #[derive(Deserialize)]
        pub struct OldGroup {
            pub id: String,
            #[serde(default)]
            pub r#type: Option<String>,
        }
        #[derive(Deserialize)]
        pub struct OldStep {
            #[serde(default)]
            pub id: Option<String>,
            #[serde(default)]
            pub group: Option<String>,
            #[serde(default)]
            pub function: Option<String>,
            #[serde(default)]
            pub command: Option<String>,
            #[serde(default)]
            pub inputs: Vec<String>,
            #[serde(default)]
            pub reads: Option<String>,
            #[serde(default)]
            pub locks: Option<Vec<String>>,
        }

        pub struct Gave {
            pub reads_pinned: bool,
            pub locks: Vec<String>,
            pub verbs: Vec<&'static str>,
        }

        impl OldStep {
            pub fn id(&self) -> String {
                match (&self.group, &self.function) {
                    (Some(g), Some(f)) => format!("{g}/{f}"),
                    _ => self.id.clone().unwrap(),
                }
            }
        }

        /// `spec_of`, `datalib/backend/dag/src/config.rs:2017-2032`, and
        /// `default_for`, `datalib/backend/dag/src/supervisor/locks.rs:93-101`.
        pub fn gave(s: &OldStep, old: &Old) -> Gave {
            let runs = crate::retired::names_datalib_step(s.command.as_deref());
            let group_type = match (&s.group, &s.function) {
                (Some(g), Some(_)) => old
                    .groups
                    .iter()
                    .find(|x| &x.id == g)
                    .and_then(|x| x.r#type.as_deref()),
                _ => None,
            };
            let f = s.function.as_deref();
            let unpinned = UNPINNED
                .iter()
                .any(|&(t, uf)| f == Some(uf) && (t.is_none() || t == group_type));
            let reads_pinned = match &s.reads {
                Some(r) => r == "pinned",
                None => !runs || !unpinned,
            };
            let default = if s.inputs.is_empty() {
                "network"
            } else if group_type.is_none() {
                "index"
            } else {
                "cpu"
            };
            let builtin = LOCKS
                .iter()
                .find(|&&(lf, _)| f == Some(lf))
                .map(|&(_, l)| l);
            let locks = match (&s.locks, runs) {
                (Some(l), _) => l.clone(),
                (None, true) => vec![builtin.unwrap_or(default).to_string()],
                (None, false) => vec![default.to_string()],
            };
            let verbs = if runs {
                vec!["reset", "prepare", "compact", "collect"]
            } else {
                vec![]
            };
            Gave {
                reads_pinned,
                locks,
                verbs,
            }
        }
    }

    const SHIPPED: &str = "datalib/backend/migrate_config/testdata/shipped_before_explicit_steps";

    /// Every config this repo shipped in the old shape rewrites into one
    /// the runner loads whole, whose steps hold the locks, read the way
    /// and take the verbs the old runner gave them. The one change is
    /// meant: a fan-in with no inputs yet holds `index`, so it is right
    /// once a source is added, where the old runner gave it `network`.
    #[test]
    fn every_shipped_config_keeps_what_the_old_runner_gave_its_steps() {
        let mut files: Vec<_> = std::fs::read_dir(SHIPPED)
            .unwrap_or_else(|e| panic!("{SHIPPED}: {e}"))
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        assert!(files.len() >= 90, "only {} fixtures", files.len());
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            let name = path.display();
            assert!(is_retired(&text).unwrap(), "{name}");
            let out = crate::upgrade_explicit_steps(&text)
                .unwrap_or_else(|e| panic!("{name}: {e:#}"))
                .unwrap_or_else(|| panic!("{name}: nothing to do"));
            assert!(!is_retired(&out).unwrap(), "{name}");
            let check = datalib_dag::config::check_text(&out);
            assert!(
                check.nothing_dropped(),
                "{name}:\n{}\n{out}",
                check.render("config.toml".as_ref())
            );
            let old: old_runner::Old = toml::from_str(&text).unwrap();
            assert_eq!(check.graph.steps.len(), old.steps.len(), "{name}");
            for s in &old.steps {
                let id = s.id();
                let spec = &check.graph.steps[*check
                    .graph
                    .by_id
                    .get(id.as_str())
                    .unwrap_or_else(|| panic!("{name}: no step {id}"))];
                let mut gave = old_runner::gave(s, &old);
                let runs = crate::retired::names_datalib_step(s.command.as_deref());
                let typed = s
                    .group
                    .as_ref()
                    .is_some_and(|g| old.groups.iter().any(|x| &x.id == g && x.r#type.is_some()));
                if runs && s.inputs.is_empty() && !typed && s.function.as_deref() != Some(INGEST) {
                    assert_eq!(gave.locks, ["network"], "{name}: {id}");
                    gave.locks = vec![INDEX.to_string()];
                }
                let held: Vec<String> = datalib_dag::supervisor::locks::held_by(spec)
                    .into_iter()
                    .map(|(l, _)| l)
                    .collect();
                assert_eq!(held, gave.locks, "{name}: {id}");
                if !s.inputs.is_empty() {
                    assert_eq!(spec.reads_pinned, gave.reads_pinned, "{name}: {id}");
                }
                let verbs: Vec<&str> = spec.verbs.iter().map(|v| v.as_str()).collect();
                assert_eq!(verbs, gave.verbs, "{name}: {id}");
            }
        }
    }
}
