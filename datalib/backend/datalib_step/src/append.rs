//! `datalib-step examples <type> --append <config.toml>`: add one source's
//! minimal stanza to a config, above the index groups, and add its steps
//! to the `inputs` of the index steps that read them by id. A config
//! without the grid and search index (`datalib-pipeline init` writes one with
//! no steps) gets it, and one without the `[[programs]]` and `[[locks]]` entries its
//! datalib-step steps need gets those. The rest of the file stays as
//! written, comments included; the result is checked the way
//! `datalib-pipeline check` checks a config before it is written.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

use datalib_dag::config::DagConfig;
use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::entry_ranges;
use datalib_dag::input_pattern::names;
use datalib_obs::status_line;
use datalib_runtime::layout::UNIFIED_GRID_AND_SEARCH_INDEX_DIR;
use datalib_source_common::ExampleKind;
use serde::Deserialize;

use crate::function::Function;
use crate::methods::ingest_methods;
use crate::source_docs::{source_docs, with_group_id};
use crate::source_type::SourceType;
use crate::step_entry::{self, TIME_SERIES_INDEX_GROUP};

/// Which index step reads a source step of each function, as
/// `(source function, index group, index function)`. Each reads them by
/// pattern (`*/render_markdown`), so a config that kept the pattern needs
/// no edit to read a source added later.
const FAN_INS: &[(Function, &str, Function)] = &[
    (
        Function::RenderMarkdown,
        UNIFIED_GRID_AND_SEARCH_INDEX_DIR,
        Function::UnifiedGridAndSearchIndex,
    ),
    (
        Function::Embed,
        UNIFIED_GRID_AND_SEARCH_INDEX_DIR,
        Function::EmbeddingMap,
    ),
    (
        Function::RenderTimeSeries,
        TIME_SERIES_INDEX_GROUP,
        Function::TimeSeriesIndex,
    ),
];

fn index_step_id() -> String {
    format!("{UNIFIED_GRID_AND_SEARCH_INDEX_DIR}/{UNIFIED_GRID_AND_SEARCH_INDEX_DIR}")
}

const INDEX_GROUPS: &[&str] = &[UNIFIED_GRID_AND_SEARCH_INDEX_DIR, TIME_SERIES_INDEX_GROUP];

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    groups: Vec<GroupView>,
    #[serde(default)]
    steps: Vec<StepView>,
    #[serde(default)]
    applets: Vec<AppletView>,
    #[serde(default)]
    programs: Vec<ProgramView>,
    #[serde(default)]
    locks: Vec<LockView>,
}

#[derive(Deserialize)]
struct GroupView {
    id: String,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
    #[serde(default)]
    locks: Option<toml::Value>,
}

#[derive(Deserialize)]
struct AppletView {
    #[serde(default)]
    group: Option<String>,
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
    fn group_id(&self) -> Option<String> {
        Some(self.id.as_deref()?.split('/').next()?.to_string())
    }

    /// What datalib-step does for it, when its command runs datalib-step.
    fn function(&self) -> Option<Function> {
        let args = step_entry::run_args(self.command.as_deref()?)?.ok()?;
        Function::parse(&args.function)
    }

    /// The program word of a command that runs `datalib-step run`.
    fn datalib_step_program(&self) -> Option<String> {
        let command = self.command.as_deref()?;
        step_entry::run_args(command)?.ok()?;
        shlex::split(command)?.into_iter().next()
    }

    fn lock_names(&self) -> Vec<String> {
        match &self.locks {
            Some(toml::Value::Array(names)) => names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect(),
            Some(toml::Value::Table(held)) => held.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

/// What appending did: the new text, and what was wired where.
#[derive(Debug)]
pub struct Appended {
    pub text: String,
    pub group: String,
    /// `(source step, index step)` for each input added.
    pub wired: Vec<(String, String)>,
    /// Source steps no index step in the config reads, with the one that
    /// would have: a config with no map has no `embedding_map`.
    pub unread: Vec<(String, String)>,
    /// The grid and search index was added, since the config had none.
    pub added_unified_index: bool,
    /// The time-series index was added, since the config had none.
    pub added_time_series_index: bool,
    /// The config has the `unified_grid_and_search_index` step an `embed` reads, and the stanza has
    /// none, so the source is found by its words alone until one is added.
    pub found_by_words_alone: bool,
}

/// The minimal stanza for `t` by `method` (needed only when `t` has more
/// than one), with its group renamed to `id` when one is given.
pub fn stanza(
    t: SourceType,
    method: Option<&str>,
    id: Option<&str>,
) -> Result<(String, &'static str), String> {
    let methods = ingest_methods(t);
    let names: Vec<&str> = methods.iter().map(|m| m.path).collect();
    let method =
        match (method, names.as_slice()) {
            (Some(m), _) => names.iter().copied().find(|n| *n == m).ok_or_else(|| {
                format!("{t} has no method {m:?}; its methods: {}", names.join(", "))
            })?,
            (None, [only]) => only,
            (None, _) => {
                return Err(format!(
                    "{t} is reached {} ways; say which with --method ({})",
                    names.len(),
                    names.join(", ")
                ))
            }
        };
    let example = source_docs(t)
        .example(ExampleKind::Minimal, method)
        .ok_or_else(|| format!("{t} has no minimal example for `{method}`"))?;
    let view: View = toml::from_str(example.toml).map_err(|e| e.message().to_string())?;
    let [group] = view.groups.as_slice() else {
        return Err(format!(
            "the {t} `{method}` example declares {} groups, not one",
            view.groups.len()
        ));
    };
    let text = match id {
        Some(id) => with_group_id(example.toml, &group.id, id),
        None => example.toml.to_string(),
    };
    Ok((text, method))
}

/// The grid and search index as datalib's app writes it into a new
/// library: its group, the index step reading every source's render by
/// pattern, and the applets every app needs.
pub fn unified_index_block() -> String {
    let g = UNIFIED_GRID_AND_SEARCH_INDEX_DIR;
    format!(
        "# ── the unified grid and search index ──────────────────────────────────\n\
         # Every source feeds this one: the grid reads its rendered markdown\n\
         # and puts it in the keyword search. Sources go above this block.\n\
         \n\
         [[groups]]\nid = \"{g}\"\nname = \"Grid and Search Index\"\n\n{}\n\
         [[applets]]\ngroup = \"{g}\"\nid = \"{g}\"\ncommand = \"datalib-applet {g}\"\n\n\
         [[applets]]\ngroup = \"{g}\"\nid = \"datalib_contacts\"\n\
         command = \"datalib-applet datalib_contacts\"\n",
        step_entry::entry(
            g,
            Function::UnifiedGridAndSearchIndex,
            None,
            &["*/render_markdown"]
        ),
    )
}

/// The time-series index reading every source's time series by pattern,
/// and the applet that serves it.
fn time_series_index_block() -> String {
    let g = TIME_SERIES_INDEX_GROUP;
    format!(
        "# ── the time-series index: every source's time series, in one list ──\n\
         [[groups]]\nid = \"{g}\"\nname = \"Time Series Index\"\n\n{}\n\
         [[applets]]\ngroup = \"{g}\"\nid = \"{g}\"\ncommand = \"datalib-applet {g}\"\n",
        step_entry::entry(
            g,
            Function::TimeSeriesIndex,
            None,
            &["*/render_time_series"]
        ),
    )
}

/// `text` with `block` after everything it holds, one blank line between.
fn with_block_at_end(text: &str, block: &str) -> String {
    let gap = if text.trim().is_empty() || text.ends_with("\n\n") {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let text = if text.trim().is_empty() { "" } else { text };
    format!("{text}{gap}{}\n", block.trim_end())
}

/// `config` with `stanza` (one `[[groups]]` entry and its steps) added
/// above the index groups and wired into the index steps that read it.
/// Refused when the config already has the stanza's group.
pub fn append(config: &str, stanza: &str) -> Result<Appended, String> {
    let have: View =
        toml::from_str(config).map_err(|e| format!("the config is not TOML: {}", e.message()))?;
    let adding: View = toml::from_str(stanza).map_err(|e| e.message().to_string())?;
    let [group] = adding.groups.as_slice() else {
        return Err("a stanza declares exactly one group".to_string());
    };
    let group = group.id.clone();
    if have.groups.iter().any(|g| g.id == group)
        || have
            .steps
            .iter()
            .any(|s| s.group_id().as_deref() == Some(group.as_str()))
    {
        return Err(format!(
            "the config already has a group {group:?}; pass --id to give this one another"
        ));
    }
    let has_unified_index = have
        .groups
        .iter()
        .any(|g| g.id == UNIFIED_GRID_AND_SEARCH_INDEX_DIR)
        || have
            .steps
            .iter()
            .any(|s| s.group_id().as_deref() == Some(UNIFIED_GRID_AND_SEARCH_INDEX_DIR));
    // Where the index begins when this adds it: the comments above it in
    // the file are not the index's, and stay above the stanza.
    let (config, added_index_at) = if has_unified_index {
        (config.to_string(), None)
    } else {
        let block = unified_index_block();
        let with = with_block_at_end(config, &block);
        let at = with.len() - block.trim_end().len() - 1;
        (with, Some(at))
    };
    let have: View = toml::from_str(&config).map_err(|e| e.message().to_string())?;

    let config_has_search = have
        .steps
        .iter()
        .any(|s| s.id.as_deref() == Some(index_step_id().as_str()));
    let has = |function: Function| adding.steps.iter().any(|s| s.function() == Some(function));
    let found_by_words_alone =
        config_has_search && has(Function::RenderMarkdown) && !has(Function::Embed);

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut wired = Vec::new();
    let mut unread = Vec::new();
    let mut needs_time_series_index = false;
    for (index_group, index_function) in index_steps() {
        let reads: Vec<String> = adding
            .steps
            .iter()
            .filter(|s| {
                FAN_INS.iter().any(|&(f, g, i)| {
                    s.function() == Some(f) && g == index_group && i == index_function
                })
            })
            .filter_map(|s| s.id.clone())
            .collect();
        if reads.is_empty() {
            continue;
        }
        let index_id = format!("{index_group}/{index_function}");
        let index = have
            .steps
            .iter()
            .find(|s| s.id.as_deref() == Some(index_id.as_str()));
        match index.and_then(|s| s.inputs.as_ref()) {
            Some(inputs) => {
                // A pattern already there (`*/render_markdown`) reads the
                // new step without an edit.
                let unnamed: Vec<String> = reads
                    .iter()
                    .filter(|r| !inputs.get_ref().iter().any(|i| names(i, r)))
                    .cloned()
                    .collect();
                if !unnamed.is_empty() {
                    let mut after = inputs.get_ref().clone();
                    after.extend(unnamed);
                    edits.push((
                        inputs.span(),
                        edit_string_array(&config[inputs.span()], &after)?,
                    ));
                }
                wired.extend(reads.into_iter().map(|r| (r, index_id.clone())));
            }
            None if index_group == TIME_SERIES_INDEX_GROUP
                && !have.groups.iter().any(|g| g.id == TIME_SERIES_INDEX_GROUP) =>
            {
                needs_time_series_index = true;
                wired.extend(reads.into_iter().map(|r| (r, index_id.clone())));
            }
            None => unread.extend(reads.into_iter().map(|r| (r, index_id.clone()))),
        }
    }

    let at = added_index_at.or_else(|| {
        entry_ranges(&config)
            .into_iter()
            .find(|r| is_index_entry(&config[r.clone()]))
            // An entry's range opens with the blank lines above it, which
            // stay above the stanza.
            .map(|r| r.end - config[r].trim_start_matches(['\n', '\r']).len())
    });
    let stanza = stanza.trim_end();
    let insert = match at {
        Some(at) => (at, format!("{stanza}\n\n")),
        None => {
            let gap = if config.trim().is_empty() || config.ends_with("\n\n") {
                ""
            } else if config.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            (config.len(), format!("{gap}{stanza}\n"))
        }
    };
    edits.push((insert.0..insert.0, insert.1));
    // Spans index the original text, so the last goes in first; the
    // stanza sits above every index entry, so ahead of every inputs edit.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut text = config.clone();
    for (span, with) in edits {
        text.replace_range(span, &with);
    }

    if needs_time_series_index {
        text = with_block_at_end(&text, &time_series_index_block());
    }
    let text = with_programs_and_locks(&text, stanza)?;
    Ok(Appended {
        text,
        group,
        wired,
        unread,
        added_unified_index: !has_unified_index,
        added_time_series_index: needs_time_series_index,
        found_by_words_alone,
    })
}

/// `text` with a `[[programs]]` entry for each spelling of datalib-step
/// its steps run, and a `[[locks]]` entry for each of datalib-step's locks
/// a step holds, where the file has none, just above `stanza`.
fn with_programs_and_locks(text: &str, stanza: &str) -> Result<String, String> {
    let view: View = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let mut programs: BTreeSet<String> = BTreeSet::new();
    let mut locks: BTreeSet<String> = BTreeSet::new();
    for step in &view.steps {
        if let Some(program) = step.datalib_step_program() {
            programs.insert(program);
        }
        locks.extend(step.lock_names());
    }
    let mut add = String::new();
    for program in programs
        .iter()
        .filter(|p| !view.programs.iter().any(|have| &have.command == *p))
    {
        let entry = step_entry::programs_entry().replace(
            &format!("command = \"{}\"", step_entry::PROGRAM),
            &format!("command = {}", toml::Value::String(program.clone())),
        );
        add.push_str(&entry);
        add.push('\n');
    }
    for name in locks
        .iter()
        .filter(|n| !view.locks.iter().any(|have| &have.name == *n))
    {
        if let Some(entry) = step_entry::locks_entry(name) {
            add.push_str(&entry);
            add.push('\n');
        }
    }
    if add.is_empty() {
        return Ok(text.to_string());
    }
    let at = text
        .find(stanza)
        .ok_or("the stanza is not in the text it was added to")?;
    let mut out = text.to_string();
    out.insert_str(at, &add);
    Ok(out)
}

/// The index steps, each once, in [`FAN_INS`] order.
fn index_steps() -> Vec<(&'static str, Function)> {
    let mut out: Vec<(&str, Function)> = Vec::new();
    for &(_, g, f) in FAN_INS {
        if !out.contains(&(g, f)) {
            out.push((g, f));
        }
    }
    out
}

/// Whether one entry of the file belongs to an index group.
fn is_index_entry(entry: &str) -> bool {
    let Ok(view) = toml::from_str::<View>(entry) else {
        return false;
    };
    let group = view
        .groups
        .first()
        .map(|g| g.id.clone())
        .or_else(|| view.steps.first().and_then(StepView::group_id))
        .or_else(|| view.applets.first().and_then(|a| a.group.clone()));
    group.is_some_and(|g| INDEX_GROUPS.contains(&g.as_str()))
}

/// What `datalib-pipeline check` would refuse in `after` that it did not
/// refuse in `before`: an entry it drops, or a step of `group` whose
/// params a sync would refuse. Rendered against `path`, one per line.
pub fn new_problems(path: &Path, before: &str, after: &str, group: &str) -> Vec<String> {
    let was = datalib_dag::config::check_text(before);
    let now = datalib_dag::config::check_text(after);
    let mut out = Vec::new();
    if now.is_fatal() || now.dropped() > was.dropped() {
        out.push(now.render(path));
    }
    let data_root = datalib_dag::config::data_root_of(path, &now.cfg);
    out.extend(
        params_problems(&now.cfg, &data_root, group)
            .into_iter()
            .map(|p| format!("{}: step {}: {}", path.display(), p.step, p.message)),
    );
    out
}

fn params_problems(
    cfg: &DagConfig,
    data_root: &Path,
    group: &str,
) -> Vec<crate::check_params::Problem> {
    let prefix = format!("{group}/");
    crate::check_params::problems(cfg, data_root)
        .into_iter()
        .filter(|p| p.step.starts_with(&prefix))
        .collect()
}

/// Exit code: 0 when the source was appended, 1 when nothing was written.
pub fn run_cli(word: &str, path: &Path, method: Option<&str>, id: Option<&str>) -> i32 {
    let shown = path.display();
    let Some(t) = SourceType::parse(word) else {
        status_line!("error: no source type {word:?}. `datalib-step help` lists them.");
        return 1;
    };
    let (stanza, method) = match stanza(t, method, id) {
        Ok(s) => s,
        Err(e) => {
            status_line!("error: {e}");
            return 1;
        }
    };
    let config = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            status_line!(
                "error: could not read {shown}: {e}. `datalib-pipeline init <data_root>` writes an \
                 empty one."
            );
            return 1;
        }
    };
    let appended = match append(&config, &stanza) {
        Ok(a) => a,
        Err(e) => {
            status_line!("error: {shown}: {e}");
            return 1;
        }
    };
    let problems = new_problems(path, &config, &appended.text, &appended.group);
    if !problems.is_empty() {
        status_line!(
            "error: {shown} would not check clean with {t} added, so it was left as it is:\n{}",
            problems.join("\n")
        );
        return 1;
    }
    let bak = match crate::config_backup::keep(path, &config).and_then(|bak| {
        datalib_runtime::atomic::write_owner_only(path, appended.text.as_bytes()).map(|()| bak)
    }) {
        Ok(bak) => bak,
        Err(e) => {
            status_line!("error: could not rewrite {shown}: {e}");
            return 1;
        }
    };
    let group = &appended.group;
    status_line!(
        "Added {t} by `{method}` to {shown} as group {group:?}. The previous file is {}.",
        bak.display()
    );
    if appended.added_unified_index {
        status_line!("Added the grid and search index, which the config did not have.");
    }
    if appended.added_time_series_index {
        status_line!("Added the time-series index, which the config did not have.");
    }
    for (step, index) in &appended.wired {
        status_line!("  {index} reads {step}");
    }
    for (step, index) in &appended.unread {
        status_line!(
            "  nothing reads {step}: the config has no {index}, so it is not indexed there"
        );
    }
    if appended.found_by_words_alone {
        status_line!(
            "  search finds it by its words, not yet by its meaning: the minimal stanza has \
             no `{group}/embed` step. Copy it from the complete stanza \
             (`datalib-step examples {t} --method {method}`)."
        );
    }
    status_line!(
        "Next: set its params (`datalib-step help {t} {method}`), sign it in if it needs \
         it (`datalib-step help {t}`), then `datalib-pipeline check {shown}`, \
         `datalib-step check-params {shown}` and \
         `datalib-pipeline sync {shown} {group}/ingest --by <you>`."
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `datalib-pipeline init` writes: comments, and no entry.
    const EMPTY: &str = datalib_dag::starter::STARTER_CONFIG;

    /// Each append kept the previous file as `<file>.bak`, so three in a
    /// row left only the state before the third, and the starter config
    /// was gone. Each now keeps its own.
    #[test]
    fn every_append_keeps_the_file_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EMPTY).unwrap();
        for id in ["slack_a", "slack_b", "slack_c"] {
            assert_eq!(run_cli("slack", &path, None, Some(id)), 0, "{id}");
        }
        let mut kept: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p != &path)
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        kept.sort_by_key(String::len);
        assert_eq!(kept.len(), 3, "one backup per append");
        assert_eq!(kept[0], EMPTY, "the first holds the starter config");
    }

    /// Loads with nothing to report, and reads in data-flow order: the
    /// stanza went where `topo-sort-config` would leave it.
    fn checks_clean(text: &str) {
        let checked = datalib_dag::config::check_text(text);
        assert!(
            checked.is_clean(),
            "{}\n---\n{text}",
            checked.render(Path::new("config.toml"))
        );
        assert_eq!(
            datalib_dag::config_order::sort_config(text),
            Ok(None),
            "{text}"
        );
    }

    fn inputs_of(text: &str, step: &str) -> Vec<String> {
        let checked = datalib_dag::config::check_text(text);
        let s = checked
            .cfg
            .steps
            .iter()
            .find(|s| s.id == step)
            .unwrap_or_else(|| panic!("no {step}"));
        s.inputs.clone()
    }

    /// A config whose grid and search index is already there, as the
    /// app's starter has it.
    fn with_the_index() -> String {
        let text = with_block_at_end(EMPTY, &unified_index_block());
        with_programs_and_locks(&text, "# ── the unified grid and search index").unwrap()
    }

    /// The case the command exists for: a source added to a new root
    /// reaches the grid. `datalib-pipeline init` writes no steps, so the grid
    /// and search index comes with the first source, and the
    /// `[[programs]]` and `[[locks]]` entries its steps need.
    #[test]
    fn a_source_on_an_empty_config_brings_the_unified_index() {
        let (stanza, method) = stanza(SourceType::Slack, None, None).unwrap();
        assert_eq!(method, "api");
        let appended = append(EMPTY, &stanza).unwrap();
        checks_clean(&appended.text);
        assert!(appended.added_unified_index);
        assert_eq!(
            inputs_of(
                &appended.text,
                "unified_grid_and_search_index/unified_grid_and_search_index"
            ),
            ["slack/render_markdown"]
        );
        assert!(
            appended.found_by_words_alone,
            "the starter searches by meaning and the minimal stanza has no embed"
        );
        assert_eq!(
            appended.wired,
            [(
                "slack/render_markdown".to_string(),
                "unified_grid_and_search_index/unified_grid_and_search_index".to_string()
            )]
        );
        let cfg = datalib_dag::config::check_text(&appended.text).cfg;
        assert_eq!(cfg.programs.len(), 1);
        assert_eq!(cfg.programs[0].command, "datalib-step");
        let locks: Vec<&str> = cfg.locks.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(locks, ["index"]);
        assert_eq!(cfg.applets.len(), 2);
        let source_at = appended.text.find("[[groups]]\nid = \"slack\"").unwrap();
        let index_at = appended
            .text
            .find("# ── the unified grid and search index")
            .unwrap();
        assert!(source_at < index_at, "{}", appended.text);
        assert!(
            appended.text.starts_with(EMPTY.trim_end()),
            "{}",
            appended.text
        );
    }

    /// On a config that has the index, the stanza goes above it, and the
    /// index is untouched: its pattern reads the new source.
    #[test]
    fn a_source_on_a_config_with_the_index_is_wired_into_the_grid() {
        let config = with_the_index();
        checks_clean(&config);
        let (stanza, _) = stanza(SourceType::Slack, None, None).unwrap();
        let appended = append(&config, &stanza).unwrap();
        checks_clean(&appended.text);
        assert!(!appended.added_unified_index);
        let marker = "# ── the unified grid and search index";
        let index_at = appended.text.find(marker).unwrap();
        let was_at = config.find(marker).unwrap();
        assert_eq!(appended.text[index_at..], config[was_at..]);
        assert_eq!(
            inputs_of(
                &appended.text,
                "unified_grid_and_search_index/unified_grid_and_search_index"
            ),
            ["slack/render_markdown"]
        );
    }

    #[test]
    fn a_second_source_goes_beside_the_first_and_keeps_the_hand_layout() {
        let config = "\
# my sources
[[programs]]
command = \"datalib-step\"
verbs = [\"reset\", \"prepare\", \"compact\", \"collect\"]

[[locks]]
name = \"index\"
slots = 2

[[groups]]
id = \"slack\"
type = \"slack\"

[[steps]]
id = \"slack/ingest\"
command = \"datalib-step run ingest slack\"

[steps.params.api]

[[steps]]
id = \"slack/render_markdown\"
command = \"datalib-step run render_markdown slack\"
inputs = [\"slack/ingest\"]
reads = \"pinned\"

[[groups]]
id = \"unified_grid_and_search_index\"

[[steps]]
id = \"unified_grid_and_search_index/unified_grid_and_search_index\"
command = \"datalib-step run unified_grid_and_search_index\"
reads = \"pinned\"
locks = [\"index\"]
inputs = [
  \"slack/render_markdown\", # the work one
]
";
        checks_clean(config);
        let (stanza, _) = stanza(SourceType::Email, Some("gmail"), Some("work-mail")).unwrap();
        let appended = append(config, &stanza).unwrap();
        checks_clean(&appended.text);
        assert!(appended
            .text
            .starts_with(config.split("[[groups]]").next().unwrap()));
        assert!(
            appended.text.contains(
                "  \"slack/render_markdown\", # the work one\n  \"work-mail/render_markdown\",\n]"
            ),
            "{}",
            appended.text
        );
        assert_eq!(appended.group, "work-mail");
        // No embedding_map here, and the minimal stanza has no embed for
        // one to read; the `unified_grid_and_search_index` step searches it by its words.
        assert!(appended.unread.is_empty());
        assert!(!appended.added_unified_index);
        assert!(appended.found_by_words_alone);
    }

    #[test]
    fn a_group_already_in_the_config_is_refused() {
        let (slack, _) = stanza(SourceType::Slack, None, None).unwrap();
        let once = append(EMPTY, &slack).unwrap().text;
        let again = append(&once, &slack).unwrap_err();
        assert!(again.contains("already has a group \"slack\""), "{again}");
        let (renamed, _) = stanza(SourceType::Slack, None, Some("slack-work")).unwrap();
        let twice = append(&once, &renamed).unwrap().text;
        checks_clean(&twice);
        // One blank line between the two stanzas and above the index
        // block, however many sources are appended.
        assert!(
            twice.contains("reads = \"pinned\"\n\n# Slack over its API"),
            "{twice}"
        );
        assert!(!twice.contains("\n\n\n"), "{twice}");
    }

    #[test]
    fn a_source_reached_several_ways_needs_its_method_named() {
        let err = stanza(SourceType::Email, None, None).unwrap_err();
        assert!(err.contains("--method") && err.contains("gmail"), "{err}");
        let err = stanza(SourceType::Email, Some("imap"), None).unwrap_err();
        assert!(err.contains("no method \"imap\""), "{err}");
    }

    /// A time-series source on a root that has no time-series index gets
    /// one, reading its `render_time_series`, and only the one such step.
    #[test]
    fn a_time_series_source_brings_the_time_series_index() {
        let config = with_the_index();
        let (garmin, _) = stanza(SourceType::Garmin, None, None).unwrap();
        let appended = append(&config, &garmin).unwrap();
        checks_clean(&appended.text);
        assert!(appended.added_time_series_index);
        assert_eq!(
            inputs_of(
                &appended.text,
                "unified_time_series_index/time_series_index"
            ),
            ["garmin/render_time_series"]
        );
        assert_eq!(
            appended
                .text
                .matches("id = \"garmin/render_time_series\"")
                .count(),
            1
        );
        // A second one is wired into the index the first brought.
        let (yolink, _) = stanza(SourceType::Yolink, None, None).unwrap();
        let both = append(&appended.text, &yolink).unwrap();
        checks_clean(&both.text);
        assert!(!both.added_time_series_index);
        assert_eq!(
            inputs_of(&both.text, "unified_time_series_index/time_series_index"),
            ["garmin/render_time_series", "yolink/render_time_series"]
        );
    }

    /// Every source's minimal stanza, for every method, appends to an
    /// empty config and to one with the index, and checks clean, params
    /// included.
    #[test]
    fn every_minimal_stanza_appends_clean() {
        use strum::VariantArray;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("config.toml");
        let indexed = with_the_index();
        for &t in SourceType::VARIANTS {
            for m in ingest_methods(t) {
                let (stanza, _) = stanza(t, Some(m.path), Some("added")).unwrap();
                for config in [EMPTY, indexed.as_str()] {
                    let appended =
                        append(config, &stanza).unwrap_or_else(|e| panic!("{t} {}: {e}", m.path));
                    checks_clean(&appended.text);
                    let problems = new_problems(&path, config, &appended.text, "added");
                    assert!(problems.is_empty(), "{t} {}: {problems:?}", m.path);
                    let renders = stanza.contains("datalib-step run render_markdown");
                    let wired = appended
                        .wired
                        .iter()
                        .any(|(s, _)| s == "added/render_markdown");
                    assert_eq!(renders, wired, "{t} {}: {:?}", m.path, appended.wired);
                }
            }
        }
    }
}
