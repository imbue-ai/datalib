//! A config as the runner runs it: the entries `datalib_config` kept,
//! turned into [`StepSpec`]s and the [`Graph`] over them, with what the
//! graph drops added to the file's diagnostics. Reading the file and the
//! rules for each entry are `datalib_config`'s; this is what the runner
//! adds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_config::input_pattern::Expansion;
use datalib_config::{
    data_root_of, entries_of, expand_inputs, validate_steps, DagConfig, Diagnostic, Reads,
    Severity, StartWhen, StepEntry, StepLocks, Verb,
};

use crate::graph::Graph;
use crate::step::{StepRun, StepSpec};
use crate::ArtifactPath;

/// The steps of a config that [`datalib_config::parse`] accepted, as the
/// runner runs them. Note what this does *not* do: it resolves nothing
/// between steps, so an `inputs` entry naming no declared step passes here
/// and is caught by [`Graph::build`].
pub fn to_specs(cfg: &DagConfig) -> Result<Vec<StepSpec>> {
    validate_steps(cfg)?;
    let universe: Vec<&str> = cfg.steps.iter().map(|s| s.id.as_str()).collect();
    let (_, specs, _) = resolve(cfg, &cfg.steps, &universe);
    Ok(specs)
}

/// `written` with every pattern in their `inputs` expanded to the ids in
/// `universe` it names, their specs reading those ids, and what each
/// pattern stood for.
fn resolve(
    cfg: &DagConfig,
    written: &[StepEntry],
    universe: &[&str],
) -> (Vec<StepEntry>, Vec<StepSpec>, Vec<Expansion>) {
    let groups: BTreeSet<&str> = cfg.groups.iter().map(|g| g.id.as_str()).collect();
    let verbs = cfg.verbs_by_program();
    let (steps, expansions) = expand_inputs(written, universe);
    let specs = written
        .iter()
        .zip(&steps)
        .map(|(w, s)| spec_of(w, &s.inputs, &groups, &verbs))
        .collect();
    (steps, specs, expansions)
}

/// The spec of a step the entry rules kept, reading `inputs`: its written
/// inputs with their patterns expanded.
fn spec_of(
    e: &StepEntry,
    inputs: &[String],
    groups: &BTreeSet<&str>,
    verbs: &BTreeMap<String, BTreeSet<Verb>>,
) -> StepSpec {
    // The entry rules refuse a step whose command, params or inputs fail
    // these, and an expansion puts only kept steps' ids in a pattern's place.
    let argv = e.argv().expect("an accepted step's command splits");
    let params = e.params_text().expect("an accepted step's params are JSON");
    let program_verbs = verbs.get(&argv[0]).cloned().unwrap_or_default();
    let mut spec = StepSpec::new(
        &e.id,
        StepRun::Subprocess {
            argv,
            env: e.env.clone(),
            params,
        },
    );
    spec.code_version = e.code_version.clone();
    spec.verbs = program_verbs;
    spec.group = e.group().filter(|g| groups.contains(g)).map(str::to_string);
    spec.reads_pinned = e.reads == Some(Reads::Pinned);
    spec.waits_for_settled_inputs = e.start == Some(StartWhen::AfterInputsSettle);
    spec.locks = e.locks.as_ref().map(StepLocks::held);
    spec.inputs_written = !e.inputs.is_empty();
    spec.inputs = inputs
        .iter()
        .map(|i| ArtifactPath::parse(i))
        .collect::<Result<_>>()
        .expect("an accepted step's inputs parse with their patterns expanded");
    spec
}

/// Read and check a config file, keeping whatever loads. Only I/O failures
/// are `Err`: a file that cannot be read has no diagnostics to give.
pub fn load_graded(path: &Path) -> Result<(ConfigCheck, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let checked = check_text(&text);
    let root = data_root_of(path, &checked.cfg);
    Ok((checked, root))
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
    /// Every pattern a step's `inputs` wrote (`*/render_markdown`), and
    /// the ids it stood for; `cfg` and `graph` hold the ids only.
    pub expansions: Vec<Expansion>,
    /// One per problem, in file order — the order someone fixing them reads.
    /// Sort by [`Severity`] for worst-first; its `Ord` ranks by how much
    /// of the config a problem drops.
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
    let universe: Vec<&str> = entries.written.iter().map(|s| s.id.as_str()).collect();
    let (_, mut specs, _) = resolve(&entries.cfg, &entries.written, &universe);
    if let Some((steps, broken_specs, expansions)) =
        without_broken_matches(&entries.cfg, &entries.written, &entries.dropped)
    {
        entries.cfg.steps = steps;
        specs = broken_specs;
        entries.expansions = expansions;
    }
    let (mut graph, mut graph_diags) = Graph::build_graded(specs, &entries.dropped);
    graph.locks = crate::supervisor::locks::declared(&entries.cfg.locks);
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
        expansions: entries.expansions,
        diagnostics: entries.diagnostics,
    }
}

/// What `datalib-pipeline check` prints for the patterns: one line each,
/// the step, the pattern and what it read.
pub fn expansion_lines(expansions: &[Expansion]) -> Vec<String> {
    expansions
        .iter()
        .map(|e| {
            let ids = if e.ids.is_empty() {
                "(no step)".to_string()
            } else {
                e.ids.join(", ")
            };
            format!("  {} reads {} = {ids}", e.step, e.pattern)
        })
        .collect()
}

/// A pattern stands for the steps that can run, so one broken source does
/// not block every step that reads its function by pattern, as it blocks a
/// step that names it. The steps the graph drops with every pattern left
/// out are the broken ones; the patterns are expanded again without them.
/// `None` when no pattern matched a broken step.
fn without_broken_matches(
    cfg: &DagConfig,
    written: &[StepEntry],
    dropped_earlier: &BTreeSet<String>,
) -> Option<(Vec<StepEntry>, Vec<StepSpec>, Vec<Expansion>)> {
    let all: Vec<&str> = written.iter().map(|w| w.id.as_str()).collect();
    let (_, expansions) = expand_inputs(written, &all);
    if expansions.is_empty() {
        return None;
    }
    let (_, named_only, _) = resolve(cfg, written, &[]);
    let (probe, _) = Graph::build_graded(named_only, dropped_earlier);
    let broken: BTreeSet<&str> = all
        .iter()
        .copied()
        .filter(|id| !probe.by_id.contains_key(*id))
        .collect();
    let hit = expansions
        .iter()
        .any(|e| e.ids.iter().any(|id| broken.contains(id.as_str())));
    if !hit {
        return None;
    }
    let runnable: Vec<&str> = all.into_iter().filter(|id| !broken.contains(id)).collect();
    Some(resolve(cfg, written, &runnable))
}

#[cfg(test)]
mod disk_floor_tests {
    use super::check_text;
    use datalib_config::disk_floor::DiskFloor;

    /// The floor is written the way a person says a size, reaches the
    /// graph the loop reads (so an edit mid-sync is used), and is on
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
    use datalib_config::*;

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
                id = "unified_grid_and_search_index"

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
    fn the_index_steps_own_unified_grid_and_search_index_by_being_it() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "slack/rendered_md", command = "r"},
                 {id = "unified_grid_and_search_index/grid", command = "a", inputs = ["slack/rendered_md"]},
                 {id = "unified_grid_and_search_index/qmd", command = "b", inputs = ["slack/rendered_md"]},
               ]"#,
        )
        .unwrap();
        let specs = to_specs(&cfg).expect("index steps must remain valid");
        crate::Graph::build(specs).expect("index steps must graph");

        // And a second claimant is refused as a duplicate, with no
        // special-case list involved.
        let clash: DagConfig = toml::from_str(
            r#"steps = [
                 {id = "unified_grid_and_search_index/grid", command = "a"},
                 {id = "unified_grid_and_search_index/grid", command = "b"},
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

    /// Guards #209: one stray key in one step costs that step, not the
    /// grid, search, the document view and every applet.
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
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "index-it"
inputs = ["pdfs/ingest"]

[[applets]]
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
"#,
        );
        // The broken step is gone and named; everything else loaded.
        assert_eq!(
            ids(&check),
            vec![
                "pdfs/ingest",
                "unified_grid_and_search_index/unified_grid_and_search_index"
            ]
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
            (
                "unified_grid_and_search_index",
                "unified_grid_and_search_index/grid",
            ),
            (
                "unified_grid_and_search_index/grid",
                "unified_grid_and_search_index",
            ),
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
    /// depends on it: `unified_grid_and_search_index` the applet sits beside
    /// `unified_grid_and_search_index/unified_grid_and_search_index` the step.
    #[test]
    fn the_scaffold_shape_loads_clean() {
        let check = check_text(
            r#"
[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "index-grid"

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
"#,
        );
        assert!(check.is_clean(), "{:?}", check.diagnostics);
        assert_eq!(check.graph.steps.len(), 1);
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
id = "unified_grid_and_search_index/grid"
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
            sev_of(&check.diagnostics, "unified_grid_and_search_index/grid"),
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
            .find(|d| d.id() == Some("unified_grid_and_search_index/grid"))
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
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"

[[applets]]
id = "2fa"
command = "x"
"#,
        );
        assert_eq!(ids(&check), vec!["good/raw"]);
        assert_eq!(check.cfg.applets.len(), 1);
        assert_eq!(check.cfg.applets[0].id, "unified_grid_and_search_index");
        assert_eq!(
            sev_of(&check.diagnostics, "system/sneaky"),
            Some(Severity::Rejected)
        );
        assert_eq!(sev_of(&check.diagnostics, "2fa"), Some(Severity::Rejected));
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
}

// --- Groups ----------------------------------------------------------------

#[cfg(test)]
mod pattern_tests {
    use super::*;

    const CONFIG: &str = r#"
[[groups]]
id = "slack"

[[steps]]
id = "slack/fetch"
command = "fetch slack"

[[steps]]
id = "slack/render"
command = "render"
inputs = ["slack/fetch"]

[[groups]]
id = "mail"

[[steps]]
id = "mail/fetch"
command = "fetch mail"

[[steps]]
id = "mail/render"
command = "render"
inputs = ["mail/fetch"]

[[steps]]
id = "index/all"
command = "index"
inputs = ["*/render"]
"#;

    fn inputs_of(check: &ConfigCheck, id: &str) -> Vec<String> {
        check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} did not load: {}", check.render("c.toml".as_ref())))
            .inputs
            .clone()
    }

    /// `*/render` reads every group's `render`, in the file's order, and
    /// everything past the loader sees the ids: the entry, the graph's
    /// edges and `--check`'s report.
    #[test]
    fn a_pattern_reads_every_step_of_its_last_part() {
        let check = check_text(CONFIG);
        assert!(check.is_clean(), "{}", check.render("c.toml".as_ref()));
        assert_eq!(
            inputs_of(&check, "index/all"),
            ["slack/render", "mail/render"]
        );
        let spec = &check.graph.steps[check.graph.by_id["index/all"]];
        let edges: Vec<&str> = spec.inputs.iter().map(|i| i.as_str()).collect();
        assert_eq!(edges, ["slack/render", "mail/render"]);
        assert_eq!(
            expansion_lines(&check.expansions),
            ["  index/all reads */render = slack/render, mail/render"]
        );
    }

    /// A group added to the file is read without touching the fan-in's
    /// entry, which is the point of writing a pattern.
    #[test]
    fn a_group_added_later_is_read_without_editing_the_fan_in() {
        let added = CONFIG.replace(
            "[[steps]]\nid = \"index/all\"",
            "[[steps]]\nid = \"notes/render\"\ncommand = \"render\"\n\n\
             [[steps]]\nid = \"index/all\"",
        );
        let check = check_text(&added);
        assert_eq!(
            inputs_of(&check, "index/all"),
            ["slack/render", "mail/render", "notes/render"]
        );
    }

    /// One group whose fetch was refused takes its own render with it, and
    /// nothing else: the fan-in that reads renders by pattern still runs,
    /// over the group that works. Named by id, the same broken render
    /// blocks the fan-in, as it always has.
    #[test]
    fn a_broken_step_does_not_block_what_reads_it_by_pattern() {
        let broken = CONFIG.replace(
            "id = \"mail/fetch\"\ncommand = \"fetch mail\"",
            "id = \"mail/fetch\"\ncommand = \"fetch mail\"\nbogus = 1",
        );
        let check = check_text(&broken);
        assert_eq!(inputs_of(&check, "index/all"), ["slack/render"]);
        assert!(check
            .diagnostics
            .iter()
            .all(|d| d.id() != Some("index/all")));
        assert_eq!(
            check
                .diagnostics
                .iter()
                .find(|d| d.id() == Some("mail/render"))
                .map(|d| d.severity),
            Some(Severity::Blocked)
        );

        let named = broken.replace(
            r#"inputs = ["*/render"]"#,
            r#"inputs = ["slack/render", "mail/render"]"#,
        );
        let check = check_text(&named);
        assert_eq!(
            check
                .diagnostics
                .iter()
                .find(|d| d.id() == Some("index/all"))
                .map(|d| d.severity),
            Some(Severity::Blocked)
        );
    }

    /// A fan-in whose pattern matches nothing reads nothing, and it is still
    /// no source: it holds `cpu`, not a `network` slot a download waits for.
    #[test]
    fn a_fan_in_with_nothing_to_read_holds_no_network_slot() {
        let text = format!(
            "{CONFIG}\n[[steps]]\nid = \"map/all\"\ncommand = \"map\"\ninputs = [\"*/embed\"]\n"
        );
        let check = check_text(&text);
        assert!(check.is_clean(), "{}", check.render("c.toml".as_ref()));
        let spec = &check.graph.steps[check.graph.by_id["map/all"]];
        assert!(spec.inputs.is_empty(), "{:?}", spec.inputs);
        assert!(!spec.is_source());
        assert_eq!(
            crate::supervisor::locks::held_by(spec),
            [(
                datalib_config::locks::CPU.to_string(),
                datalib_config::locks::Hold::Shared
            )]
        );
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use datalib_config::*;

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
id = "unified_grid_and_search_index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "index-grid"
inputs = ["work-slack/render_markdown"]

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
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
                "unified_grid_and_search_index/unified_grid_and_search_index",
                "work-slack/ingest",
                "work-slack/render_markdown"
            ]
        );
        assert_eq!(check.cfg.groups.len(), 2);
        assert_eq!(check.cfg.groups[0].name.as_deref(), Some("Work Slack"));
        assert_eq!(
            check.cfg.applets[0].group.as_deref(),
            Some("unified_grid_and_search_index")
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
        for good in ["a", "work-slack", "a.b_c", "unified_grid_and_search_index"] {
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

    /// An applet's group is a filing, so a wrong one is advice rather than a
    /// reason to take the grid down.
    #[test]
    fn an_applet_under_an_undeclared_group_only_warns() {
        let check = check_text(
            "[[applets]]\ngroup = \"nope\"\nid = \"unified_grid_and_search_index\"\ncommand = \"x\"\n",
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
    use datalib_config::locks::Hold;

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
    use datalib_config::*;

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
            ("embed_search_index = true\n", "embed_search_index"),
            ("[modal]\nmax_sandboxes = 10\n", "modal"),
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
