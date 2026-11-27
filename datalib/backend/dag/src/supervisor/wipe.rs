//! What a wipe — a reset, a purge, a compaction or a collection (`store::JobKind`) — touches, when
//! the loop may carry it out, and what it leaves in the loop's `Record`. Pure; the
//! loop (`round.rs`) stops the steps, runs the reset and deletes the trees.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::Graph;
use crate::supervisor::record::{Record, StepRecord};
use crate::supervisor::store::JobKind;

/// The steps of `graph` a wipe keeps from running: none of them may run
/// until it is done. For a reset, its targets; for a purge, every step
/// still writing under one of its groups, which is any the config still
/// names; for a compaction, the steps it runs with `--compact` and every
/// step that reads one, since a reader on a commit the fold drops would
/// be left reading nothing. A collection keeps the same steps from
/// running: it drops no commit, but it rewrites the file under a reader.
pub fn kept_from_running(graph: &Graph, kind: JobKind, targets: &[String]) -> Vec<usize> {
    match kind {
        JobKind::Compact | JobKind::Collect => {
            let Some(how) = kind.shrink() else {
                return Vec::new();
            };
            let to_shrink = compact_targets(graph, targets, how);
            let readers = to_shrink
                .iter()
                .flat_map(|&i| graph.dependents[i].iter().copied());
            let kept: BTreeSet<usize> = to_shrink.iter().copied().chain(readers).collect();
            kept.into_iter().collect()
        }
        JobKind::Reset => targets
            .iter()
            .filter_map(|t| graph.by_id.get(t).copied())
            .collect(),
        JobKind::Purge => (0..graph.steps.len())
            .filter(|&i| targets.iter().any(|g| g == tree_group(&graph.steps[i].id)))
            .collect(),
        // The prepare pass keeps from running the steps whose
        // `--prepare` has not exited itself (`round.rs`'s `PreparePass`).
        JobKind::Prepare => Vec::new(),
    }
}

/// Why the wipe cannot be done at all, or `None`. `latest` is the newest
/// graph the loop has read, whether it runs it yet or not.
pub fn refusal(latest: &Graph, kind: JobKind, targets: &[String]) -> Option<String> {
    if targets.is_empty() && kind.shrink().is_none() {
        return Some("nothing to wipe".into());
    }
    match kind {
        JobKind::Compact | JobKind::Collect => {
            targets.iter().find_map(|t| match latest.by_id.get(t) {
                None => Some(format!("the config has no step {t:?}")),
                Some(&i)
                    if !kind
                        .shrink()
                        .is_some_and(|how| latest.steps[i].takes(how.verb())) =>
                {
                    Some(format!(
                        "{t:?} does not take {}",
                        kind.shrink().map_or(String::new(), |s| s.verb().flag())
                    ))
                }
                Some(_) => None,
            })
        }
        JobKind::Reset => targets
            .iter()
            .find(|t| !latest.by_id.contains_key(*t))
            .map(|t| format!("the config has no step {t:?}")),
        JobKind::Purge => {
            if let Some(bad) = targets.iter().find(|g| !datalib_config::usable_group_id(g)) {
                return Some(format!("{bad:?} is not a group id"));
            }
            let named: BTreeSet<&str> = latest.steps.iter().map(|s| tree_group(&s.id)).collect();
            targets
                .iter()
                .find(|g| named.contains(g.as_str()))
                .map(|g| format!("the config still has {g:?}; remove it from the config first"))
        }
        JobKind::Prepare => Some("the loop opens its own prepare pass each time it starts".into()),
    }
}

/// The steps a compaction or a collection runs with its verb, in graph
/// order: its targets, or with none named, every step that takes the verb.
pub fn compact_targets(
    graph: &Graph,
    targets: &[String],
    how: crate::subprocess::Shrink,
) -> Vec<usize> {
    if targets.is_empty() {
        return graph
            .topo
            .iter()
            .copied()
            .filter(|&i| graph.steps[i].takes(how.verb()))
            .collect();
    }
    graph
        .topo
        .iter()
        .copied()
        .filter(|&i| targets.contains(&graph.steps[i].id))
        .collect()
}

/// Failures grouped by what went wrong, each naming its first few steps:
/// one cause, which a missing binary makes of every step, reads as one.
pub fn by_cause(failed: &BTreeMap<String, Vec<&str>>) -> String {
    const NAMED: usize = 3;
    failed
        .iter()
        .map(|(why, steps)| {
            let named = steps[..steps.len().min(NAMED)].join(", ");
            match steps.len().saturating_sub(NAMED) {
                0 => format!("{named} ({why})"),
                more => format!("{named} and {more} more ({why})"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a reset syncs next. A step that reads something is rebuilt from
/// it at once, and what reads it follows; a download is not refilled —
/// that is its next Sync — so only what reads it runs, and takes the
/// emptiness downstream.
pub fn after_reset(graph: &Graph, targets: &[String]) -> Vec<String> {
    let reset: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for step in &reset {
        let Some(&i) = graph.by_id.get(*step) else {
            continue;
        };
        if !graph.deps[i].is_empty() {
            roots.insert(step.to_string());
            continue;
        }
        for &d in &graph.dependents[i] {
            let id = &graph.steps[d].id;
            if !reset.contains(id.as_str()) {
                roots.insert(id.clone());
            }
        }
    }
    roots.into_iter().collect()
}

/// A step just reset: at `version`, so what reads it sees something new,
/// and with no history, so its next run starts from nothing. Whether it is
/// turned off is a person's setting, and stays.
pub fn emptied(was: Option<&StepRecord>, version: String) -> StepRecord {
    StepRecord {
        version: Some(version),
        turned_off_by: was.and_then(|w| w.turned_off_by.clone()),
        ..Default::default()
    }
}

/// Deletes from `record` the steps that write under `groups`.
pub fn remove_group_steps(record: &mut Record, groups: &[String]) {
    record
        .steps
        .retain(|step, _| !groups.iter().any(|g| g == tree_group(step)));
}

/// The group directory a step id writes under: its first segment.
pub fn tree_group(step_id: &str) -> &str {
    step_id.split('/').next().unwrap_or(step_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StepOutcome, StepRun, StepSpec};

    fn graph(steps: &[(&str, &[&str])]) -> Graph {
        let specs = steps
            .iter()
            .map(|(id, inputs)| {
                inputs.iter().fold(
                    StepSpec::new(
                        *id,
                        StepRun::in_process(|_| async { Ok(StepOutcome::default()) }),
                    ),
                    |s, i| s.input(i),
                )
            })
            .collect();
        Graph::build(specs).unwrap()
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// A reset download is not refilled — only what reads it runs — and a
    /// reset render is rebuilt from what it reads at once.
    #[test]
    fn a_reset_syncs_what_reads_a_download_and_rebuilds_a_render() {
        let g = graph(&[
            ("a/ingest", &[]),
            ("a/render", &["a/ingest"]),
            ("idx/grid", &["a/render"]),
        ]);
        let after = |ids: &[&str]| after_reset(&g, &strings(ids));
        assert_eq!(after(&["a/ingest"]), ["a/render"]);
        assert_eq!(after(&["a/render"]), ["a/render"]);
        assert_eq!(after(&["a/ingest", "a/render"]), ["a/render"]);
    }

    /// A reset holds only its targets; a purge, every step still writing
    /// under its groups — none once the config has let them go.
    #[test]
    fn a_wipe_keeps_from_running_the_steps_that_write_what_it_wipes() {
        let g = graph(&[
            ("a/ingest", &[]),
            ("a/render", &["a/ingest"]),
            ("b/ingest", &[]),
            ("idx/grid", &["a/render"]),
        ]);
        let names = |ix: Vec<usize>| -> Vec<&str> {
            ix.into_iter().map(|i| g.steps[i].id.as_str()).collect()
        };
        assert_eq!(
            names(kept_from_running(
                &g,
                JobKind::Reset,
                &strings(&["a/ingest"])
            )),
            ["a/ingest"]
        );
        let mut purged = names(kept_from_running(&g, JobKind::Purge, &strings(&["a"])));
        purged.sort();
        assert_eq!(purged, ["a/ingest", "a/render"]);
        assert!(kept_from_running(&g, JobKind::Purge, &strings(&["gone"])).is_empty());
    }

    /// A compaction keeps from running the steps it runs with `--compact`
    /// and every step reading one; with no targets it runs every step that
    /// takes the verb — that verb, not the other one.
    #[test]
    fn a_compaction_keeps_from_running_what_it_runs_and_what_reads_it() {
        let mut g = graph(&[
            ("a/ingest", &[]),
            ("a/render", &["a/ingest"]),
            ("b/render", &[]),
            ("idx/grid", &["a/render", "b/render"]),
            ("idx/qmd", &["idx/grid"]),
        ]);
        for id in ["a/ingest", "a/render", "b/render", "idx/grid"] {
            let i = g.by_id[id];
            g.steps[i].verbs.insert(datalib_config::Verb::Compact);
            if id != "b/render" {
                g.steps[i].verbs.insert(datalib_config::Verb::Collect);
            }
        }
        let names = |ix: Vec<usize>| -> Vec<String> {
            let mut out: Vec<String> = ix.into_iter().map(|i| g.steps[i].id.clone()).collect();
            out.sort();
            out
        };
        assert_eq!(
            names(kept_from_running(
                &g,
                JobKind::Compact,
                &strings(&["a/render"])
            )),
            ["a/render", "idx/grid"]
        );
        assert_eq!(
            names(compact_targets(&g, &[], crate::subprocess::Shrink::Compact)),
            ["a/ingest", "a/render", "b/render", "idx/grid"]
        );
        assert_eq!(
            names(compact_targets(&g, &[], crate::subprocess::Shrink::Collect)),
            ["a/ingest", "a/render", "idx/grid"]
        );
        assert_eq!(
            names(kept_from_running(&g, JobKind::Compact, &[])),
            ["a/ingest", "a/render", "b/render", "idx/grid", "idx/qmd"]
        );
        assert_eq!(refusal(&g, JobKind::Compact, &[]), None);
        assert!(refusal(&g, JobKind::Compact, &strings(&["idx/qmd"]))
            .unwrap()
            .contains("does not take --compact"));
        assert!(refusal(&g, JobKind::Compact, &strings(&["nope/x"]))
            .unwrap()
            .contains("no step"));

        // A collection keeps from running what it runs and what reads that.
        assert_eq!(
            names(kept_from_running(&g, JobKind::Collect, &[])),
            ["a/ingest", "a/render", "idx/grid", "idx/qmd"]
        );
        assert!(refusal(&g, JobKind::Collect, &strings(&["b/render"]))
            .unwrap()
            .contains("does not take --collect"));
        assert_eq!(refusal(&g, JobKind::Collect, &[]), None);
        assert!(refusal(&g, JobKind::Collect, &strings(&["idx/qmd"]))
            .unwrap()
            .contains("does not take --collect"));
    }

    /// One cause shared by many steps is said once.
    #[test]
    fn failures_are_said_once_per_cause() {
        let failed = BTreeMap::from([
            (
                "spawn: no such file".to_string(),
                vec!["a/render", "b/render", "c/render", "d/render", "e/render"],
            ),
            ("disk full".to_string(), vec!["idx/grid"]),
        ]);
        assert_eq!(
            by_cause(&failed),
            "idx/grid (disk full); a/render, b/render, c/render and 2 more (spawn: no such file)"
        );
    }

    /// A purge deletes a directory under the data root, so it takes only
    /// a group id — never a path, never `system` — and never a group the
    /// config still runs. A reset takes only steps the config has.
    #[test]
    fn a_wipe_is_refused_what_it_may_not_touch() {
        let g = graph(&[("keep/raw", &[])]);
        let purge = |groups: &[&str]| refusal(&g, JobKind::Purge, &strings(groups));
        assert_eq!(purge(&["gone"]), None);
        assert!(purge(&[]).is_some());
        assert!(purge(&["gone", "keep"])
            .unwrap()
            .contains("still has \"keep\""));
        for bad in ["", "..", "system", "SYSTEM", "Keep", "a/b", "../elsewhere"] {
            assert!(purge(&[bad]).unwrap().contains("not a group id"), "{bad:?}");
        }
        assert_eq!(refusal(&g, JobKind::Reset, &strings(&["keep/raw"])), None);
        assert!(refusal(&g, JobKind::Reset, &strings(&["nope/raw"]))
            .unwrap()
            .contains("no step \"nope/raw\""));
    }

    /// A reset step keeps no history, but a person's switch stays.
    #[test]
    fn a_reset_step_loses_its_runs_but_not_its_switch() {
        let was = StepRecord {
            version: Some("v1".into()),
            succeeded: true,
            fingerprint: "fp".into(),
            turned_off_by: Some("ui".into()),
            ..Default::default()
        };
        let now = emptied(Some(&was), "v2".into());
        assert_eq!(now.version.as_deref(), Some("v2"));
        assert!(!now.succeeded);
        assert_eq!(now.fingerprint, "");
        assert_eq!(now.turned_off_by.as_deref(), Some("ui"));
    }

    /// Guards the trap a bare `rm -r` sets: a group re-added under the
    /// same id read as up to date because its `steps` row still said its
    /// step had succeeded.
    #[test]
    fn a_purge_removes_every_step_of_its_groups() {
        let mut record = Record::default();
        for id in ["gone/raw", "gone/render", "keep/raw", "gonelike/raw"] {
            record.steps.insert(id.into(), StepRecord::default());
        }
        remove_group_steps(&mut record, &strings(&["gone"]));
        let left: Vec<&String> = record.steps.keys().collect();
        assert_eq!(left, ["gonelike/raw", "keep/raw"]);
    }
}
