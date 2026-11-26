//! Which steps the prepare pass asks each time the loop starts
//! (`dag/README.md` §"Upgrading a root"): every step whose program takes
//! the `--prepare` verb, to make what it wrote usable. The
//! runner knows nothing of any store or of when a program changed; each
//! step answers for its own (`step_protocol.md` § Verbs).

use crate::graph::Graph;
use crate::step::StepId;

/// The steps a prepare pass asks, producers before what reads them, so a
/// render answers after its raw store has been brought up to date.
pub fn steps_to_ask(graph: &Graph) -> Vec<StepId> {
    graph
        .topo
        .iter()
        .map(|&i| &graph.steps[i])
        .filter(|spec| spec.takes(crate::step::Verb::Prepare))
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
        if migrates {
            spec.verbs.insert(crate::step::Verb::Prepare);
        }
        spec
    }

    /// Only steps that take the verb are asked, and a reader is asked
    /// after what it reads.
    #[test]
    fn a_pass_asks_the_steps_that_prepare_in_graph_order() {
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
