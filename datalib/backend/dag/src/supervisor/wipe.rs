//! What a wipe — a reset or a purge (`store::WipeKind`) — touches, when
//! the loop may carry it out, and what it leaves in the record. Pure; the
//! loop (`round.rs`) stops the steps, runs the reset and deletes the trees.

use std::collections::BTreeSet;

use crate::graph::Graph;
use crate::supervisor::record::{Record, StepRecord};
use crate::supervisor::store::WipeKind;

/// The steps of `graph` a wipe holds: none of them may run until it is
/// done. A reset holds its targets; a purge, every step still writing
/// under one of its groups, which is any the config has not yet let go.
pub fn held(graph: &Graph, kind: WipeKind, targets: &[String]) -> Vec<usize> {
    match kind {
        WipeKind::Reset => targets
            .iter()
            .filter_map(|t| graph.by_id.get(t).copied())
            .collect(),
        WipeKind::Purge => (0..graph.steps.len())
            .filter(|&i| targets.iter().any(|g| g == tree_group(&graph.steps[i].id)))
            .collect(),
    }
}

/// Why the wipe cannot be done at all, or `None`. `latest` is the newest
/// graph the loop has heard of, taken on or not.
pub fn refusal(latest: &Graph, kind: WipeKind, targets: &[String]) -> Option<String> {
    if targets.is_empty() {
        return Some("nothing to wipe".into());
    }
    match kind {
        WipeKind::Reset => targets
            .iter()
            .find(|t| !latest.by_id.contains_key(*t))
            .map(|t| format!("the config has no step {t:?}")),
        WipeKind::Purge => {
            if let Some(bad) = targets.iter().find(|g| !crate::config::usable_group_id(g)) {
                return Some(format!("{bad:?} is not a group id"));
            }
            let named: BTreeSet<&str> = latest.steps.iter().map(|s| tree_group(&s.id)).collect();
            targets
                .iter()
                .find(|g| named.contains(g.as_str()))
                .map(|g| format!("the config still has {g:?}; remove it from the config first"))
        }
    }
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

/// The record without the steps that write under `groups`.
pub fn forget_groups(record: &mut Record, groups: &[String]) {
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
    fn a_wipe_holds_the_steps_that_write_what_it_wipes() {
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
            names(held(&g, WipeKind::Reset, &strings(&["a/ingest"]))),
            ["a/ingest"]
        );
        let mut purged = names(held(&g, WipeKind::Purge, &strings(&["a"])));
        purged.sort();
        assert_eq!(purged, ["a/ingest", "a/render"]);
        assert!(held(&g, WipeKind::Purge, &strings(&["gone"])).is_empty());
    }

    /// A purge deletes a directory under the data root, so it takes only
    /// a group id — never a path, never `system` — and never a group the
    /// config still runs. A reset takes only steps the config has.
    #[test]
    fn a_wipe_is_refused_what_it_may_not_touch() {
        let g = graph(&[("keep/raw", &[])]);
        let purge = |groups: &[&str]| refusal(&g, WipeKind::Purge, &strings(groups));
        assert_eq!(purge(&["gone"]), None);
        assert!(purge(&[]).is_some());
        assert!(purge(&["gone", "keep"])
            .unwrap()
            .contains("still has \"keep\""));
        for bad in ["", "..", "system", "a/b", "../elsewhere"] {
            assert!(purge(&[bad]).unwrap().contains("not a group id"), "{bad:?}");
        }
        assert_eq!(refusal(&g, WipeKind::Reset, &strings(&["keep/raw"])), None);
        assert!(refusal(&g, WipeKind::Reset, &strings(&["nope/raw"]))
            .unwrap()
            .contains("no step \"nope/raw\""));
    }

    /// A reset step keeps no history, but a person's switch stays.
    #[test]
    fn a_reset_step_forgets_its_runs_but_not_its_switch() {
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
    /// same id read as up to date because the record still said its step
    /// had succeeded.
    #[test]
    fn a_purge_forgets_every_step_of_its_groups() {
        let mut record = Record::default();
        for id in ["gone/raw", "gone/render", "keep/raw", "gonelike/raw"] {
            record.steps.insert(id.into(), StepRecord::default());
        }
        forget_groups(&mut record, &strings(&["gone"]));
        let left: Vec<&String> = record.steps.keys().collect();
        assert_eq!(left, ["gonelike/raw", "keep/raw"]);
    }
}
