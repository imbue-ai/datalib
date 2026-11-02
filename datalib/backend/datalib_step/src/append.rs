//! `datalib-step examples <type> --append <config.toml>`: add one source's
//! minimal stanza to a config, above the index groups, and add its steps
//! to the `inputs` of the index steps that read them. The rest of the
//! file stays as written, comments included; the result is checked the
//! way `datalib-dag --check` checks a config before it is written.

use std::ops::Range;
use std::path::Path;

use datalib_dag::config::{DagConfig, TIME_SERIES_INDEX_GROUP};
use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::entry_ranges;
use datalib_obs::status_line;
use datalib_runtime::layout::UNIFIED_DOCUMENT_INDEX_DIR;
use datalib_source_common::ExampleKind;
use serde::Deserialize;

use crate::methods::ingest_methods;
use crate::source_docs::{source_docs, with_group_id};
use crate::source_type::SourceType;

/// Which index step reads a source step of each function, as
/// `(source function, index group, index function)`.
const FAN_INS: &[(&str, &str, &str)] = &[
    ("render_markdown", UNIFIED_DOCUMENT_INDEX_DIR, "grid_index"),
    (
        "keyword_index",
        UNIFIED_DOCUMENT_INDEX_DIR,
        "qmd_aggregator",
    ),
    ("embed", UNIFIED_DOCUMENT_INDEX_DIR, "qmd_aggregator"),
    (
        "render_time_series",
        TIME_SERIES_INDEX_GROUP,
        "time_series_index",
    ),
];

const INDEX_GROUPS: &[&str] = &[UNIFIED_DOCUMENT_INDEX_DIR, TIME_SERIES_INDEX_GROUP];

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    groups: Vec<GroupView>,
    #[serde(default)]
    steps: Vec<StepView>,
    #[serde(default)]
    applets: Vec<AppletView>,
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
    group: Option<String>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
}

#[derive(Deserialize)]
struct AppletView {
    #[serde(default)]
    group: Option<String>,
}

impl StepView {
    /// `<group>/<function>`, or the id a custom step writes.
    fn step_id(&self) -> Option<String> {
        match (&self.group, &self.function) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }

    fn group_id(&self) -> Option<String> {
        self.group
            .clone()
            .or_else(|| Some(self.id.as_deref()?.split('/').next()?.to_string()))
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
    /// would have: a config with search turned off has no `qmd_aggregator`.
    pub unread: Vec<(String, String)>,
    /// The time-series index was added, since the config had none.
    pub added_time_series_index: bool,
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
                    s.function.as_deref() == Some(f) && g == index_group && i == index_function
                })
            })
            .filter_map(StepView::step_id)
            .collect();
        if reads.is_empty() {
            continue;
        }
        let index_id = format!("{index_group}/{index_function}");
        let index = have
            .steps
            .iter()
            .find(|s| s.step_id().as_deref() == Some(index_id.as_str()));
        match index.and_then(|s| s.inputs.as_ref()) {
            Some(inputs) => {
                let mut after = inputs.get_ref().clone();
                after.extend(reads.iter().cloned());
                edits.push((
                    inputs.span(),
                    edit_string_array(&config[inputs.span()], &after)?,
                ));
                wired.extend(reads.into_iter().map(|r| (r, index_id.clone())));
            }
            None if index_group == TIME_SERIES_INDEX_GROUP
                && !have.groups.iter().any(|g| g.id == TIME_SERIES_INDEX_GROUP) =>
            {
                needs_time_series_index = true;
            }
            None => unread.extend(reads.into_iter().map(|r| (r, index_id.clone()))),
        }
    }

    let at = entry_ranges(config)
        .into_iter()
        .find(|r| is_index_entry(&config[r.clone()]))
        // An entry's range opens with the blank lines above it, which
        // stay above the stanza.
        .map(|r| r.end - config[r].trim_start_matches(['\n', '\r']).len());
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
    let mut text = config.to_string();
    for (span, with) in edits {
        text.replace_range(span, &with);
    }

    let added_time_series_index = needs_time_series_index
        && match datalib_migrate_config::upgrade_time_series_steps(&text)
            .map_err(|e| format!("{e:#}"))?
        {
            Some(with_index) => {
                text = with_index;
                wired.extend(
                    adding
                        .steps
                        .iter()
                        .filter(|s| s.function.as_deref() == Some("render_time_series"))
                        .filter_map(StepView::step_id)
                        .map(|r| (r, format!("{TIME_SERIES_INDEX_GROUP}/time_series_index"))),
                );
                true
            }
            None => false,
        };
    Ok(Appended {
        text,
        group,
        wired,
        unread,
        added_time_series_index,
    })
}

/// The index steps, each once, in [`FAN_INS`] order.
fn index_steps() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&str, &str)> = Vec::new();
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

/// What `datalib-dag --check` would refuse in `after` that it did not
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
                "error: could not read {shown}: {e}. `datalib-dag init <data_root>` writes a \
                 starter config."
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
    status_line!(
        "Next: set its params (`datalib-step help {t} {method}`), sign it in if it needs \
         it (`datalib-step help {t}`), then `datalib-dag --check {shown}` and \
         `datalib-dag {shown} --sync {group}/ingest --by <you>`."
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each append kept the previous file as `<file>.bak`, so three in a
    /// row left only the state before the third, and the starter config
    /// was gone. Each now keeps its own.
    #[test]
    fn every_append_keeps_the_file_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let starter = datalib_dag::starter::STARTER_CONFIG;
        std::fs::write(&path, starter).unwrap();
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
        assert_eq!(kept[0], starter, "the first holds the starter config");
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

    /// The case the command exists for: a source added to a new root
    /// reaches the grid, which a hand-edit that forgets `grid_index`'s
    /// inputs silently does not.
    #[test]
    fn a_source_on_the_starter_config_is_wired_into_the_grid() {
        let starter = datalib_dag::starter::STARTER_CONFIG;
        let (stanza, method) = stanza(SourceType::Slack, None, None).unwrap();
        assert_eq!(method, "api");
        let appended = append(starter, &stanza).unwrap();
        checks_clean(&appended.text);
        assert_eq!(
            inputs_of(&appended.text, "unified_document_index/grid_index"),
            ["slack/render_markdown"]
        );
        assert_eq!(
            appended.wired,
            [(
                "slack/render_markdown".to_string(),
                "unified_document_index/grid_index".to_string()
            )]
        );
        // Above the index block, whose comments stay with it, and the
        // block itself untouched but for the one array.
        let source_at = appended.text.find("[[groups]]\nid = \"slack\"").unwrap();
        let index_at = appended
            .text
            .find("# ── the unified document index")
            .unwrap();
        assert!(source_at < index_at, "{}", appended.text);
        assert_eq!(
            appended.text.replace(
                "inputs = [\"slack/render_markdown\"]\n\n[[steps]]\ngroup = \"unified_document_index\"",
                "inputs = []\n\n[[steps]]\ngroup = \"unified_document_index\""
            )[index_at..],
            *starter
        );
    }

    #[test]
    fn a_second_source_goes_beside_the_first_and_keeps_the_hand_layout() {
        let config = "\
# my sources
[[groups]]
id = \"slack\"
type = \"slack\"

[[steps]]
group = \"slack\"
function = \"ingest\"

[steps.params.api]

[[steps]]
group = \"slack\"
function = \"render_markdown\"
inputs = [\"slack/ingest\"]

[[groups]]
id = \"unified_document_index\"

[[steps]]
group = \"unified_document_index\"
function = \"grid_index\"
inputs = [
  \"slack/render_markdown\", # the work one
]
";
        let (stanza, _) = stanza(SourceType::Email, Some("gmail"), Some("work-mail")).unwrap();
        let appended = append(config, &stanza).unwrap();
        checks_clean(&appended.text);
        assert!(appended.text.starts_with("# my sources\n"));
        assert!(
            appended.text.contains(
                "  \"slack/render_markdown\", # the work one\n  \"work-mail/render_markdown\",\n]"
            ),
            "{}",
            appended.text
        );
        assert_eq!(appended.group, "work-mail");
        // No qmd_aggregator here, and the minimal stanza has no search
        // steps for one to read.
        assert!(appended.unread.is_empty());
    }

    #[test]
    fn a_group_already_in_the_config_is_refused() {
        let starter = datalib_dag::starter::STARTER_CONFIG;
        let (slack, _) = stanza(SourceType::Slack, None, None).unwrap();
        let once = append(starter, &slack).unwrap().text;
        let again = append(&once, &slack).unwrap_err();
        assert!(again.contains("already has a group \"slack\""), "{again}");
        let (renamed, _) = stanza(SourceType::Slack, None, Some("slack-work")).unwrap();
        let twice = append(&once, &renamed).unwrap().text;
        checks_clean(&twice);
        // One blank line between the two stanzas and above the index
        // block, however many sources are appended.
        assert!(
            twice.contains("inputs = [\"slack/ingest\"]\n\n# Slack over its API"),
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
        let starter = datalib_dag::starter::STARTER_CONFIG;
        let (garmin, _) = stanza(SourceType::Garmin, None, None).unwrap();
        let appended = append(starter, &garmin).unwrap();
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
                .matches("function = \"render_time_series\"")
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

    /// Every source's minimal stanza, for every method, appends to the
    /// starter config and checks clean, params included.
    #[test]
    fn every_minimal_stanza_appends_clean() {
        use strum::VariantArray;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("config.toml");
        let starter = datalib_dag::starter::STARTER_CONFIG;
        for &t in SourceType::VARIANTS {
            for m in ingest_methods(t) {
                let (stanza, _) = stanza(t, Some(m.path), Some("added")).unwrap();
                let appended =
                    append(starter, &stanza).unwrap_or_else(|e| panic!("{t} {}: {e}", m.path));
                checks_clean(&appended.text);
                let problems = new_problems(&path, starter, &appended.text, "added");
                assert!(problems.is_empty(), "{t} {}: {problems:?}", m.path);
                let renders = stanza.contains("function = \"render_markdown\"");
                let wired = appended
                    .wired
                    .iter()
                    .any(|(s, _)| s == "added/render_markdown");
                assert_eq!(renders, wired, "{t} {}: {:?}", m.path, appended.wired);
            }
        }
    }
}
