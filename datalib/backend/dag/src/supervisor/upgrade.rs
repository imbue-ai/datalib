//! What a launch does before the loop takes any request, the first time a
//! build runs on a root (`docs/dev/plans/upgrade_on_launch.md`): ask every
//! step that takes the `--migrate` verb to bring what it wrote to this
//! build's shape. The runner knows nothing of any store; each step answers
//! for its own (`step_protocol.md` § Migrate).

use crate::graph::Graph;
use crate::step::StepId;

/// Which build is running, as the record of launch passes names it.
pub fn this_build() -> String {
    let version = datalib_runtime::build_id::DATALIB_VERSION;
    match datalib_runtime::build_id::git_hash() {
        Some(hash) => format!("{version}+{hash}"),
        None => version.to_string(),
    }
}

/// The steps a launch pass asks, producers before what reads them, so a
/// render answers after its raw store has been migrated.
pub fn steps_to_ask(graph: &Graph) -> Vec<StepId> {
    graph
        .topo
        .iter()
        .map(|&i| &graph.steps[i])
        .filter(|spec| spec.migrates)
        .map(|spec| spec.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::{StepOutcome, StepRun, StepSpec};

    fn step(id: &str, inputs: &[&str], migrates: bool) -> StepSpec {
        let mut spec = StepSpec::new(
            id,
            StepRun::in_process(|_| async { Ok(StepOutcome::default()) }),
        );
        for i in inputs {
            spec = spec.input(i);
        }
        spec.migrates = migrates;
        spec
    }

    /// Only steps that take the verb are asked, and a reader is asked
    /// after what it reads.
    #[test]
    fn a_pass_asks_the_steps_that_migrate_in_graph_order() {
        let graph = Graph::build(vec![
            step("mail/render_markdown", &["mail/ingest"], true),
            step("custom/out", &["mail/ingest"], false),
            step("mail/ingest", &[], true),
        ])
        .unwrap();
        assert_eq!(
            steps_to_ask(&graph),
            ["mail/ingest", "mail/render_markdown"]
        );
    }
}
