//! Graph assembly and validation.
//!
//! The DAG is **declared**, not derived: step A → step B iff B names
//! A's id in its `inputs`. A step's id is also the one tree it writes,
//! so an input is simultaneously a step reference and an artifact path
//! and nothing has to be matched against anything.
//!
//! Validation is correspondingly small:
//!
//! * ids are unique — which is also what makes single-writer true, since
//!   a step's id *is* its output tree;
//! * every input names a declared step;
//! * no step consumes its own output;
//! * no cycles.
//!
//! This module used to derive edges by testing every input pattern
//! against every output path, and to synthesize source steps for
//! producer-less inputs. Both are gone with the pattern machinery they
//! rested on — see `docs/dev/step_identity.md`.

use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Result};

use crate::artifact::ArtifactPath;
use crate::step::{StepId, StepSpec};

#[derive(Debug)]
pub struct Graph {
    pub steps: Vec<StepSpec>,
    /// Index into `steps` by id.
    pub by_id: HashMap<StepId, usize>,
    /// step idx → indexes of steps it depends on.
    pub deps: Vec<BTreeSet<usize>>,
    /// step idx → indexes of steps that depend on it.
    pub dependents: Vec<BTreeSet<usize>>,
    /// step idx → the input artifacts it reads, which are the ids of
    /// its producer steps. Kept as a field (rather than read off
    /// `steps[i].inputs`) because the scheduler diffs it against
    /// recorded versions on every pass.
    pub resolved_inputs: Vec<Vec<ArtifactPath>>,
    /// A topological order (dependencies before dependents).
    pub topo: Vec<usize>,
    /// step idx → hash of the step's own definition: id, command, env,
    /// and declared inputs (see
    /// [`StepSpec::fingerprint_material`]). Not the contents of what it
    /// reads — those are the input versions. A step whose fingerprint
    /// differs from the one recorded at its last success is stale, which
    /// is how a config edit takes effect.
    pub fingerprints: Vec<String>,
}

impl Graph {
    /// Ids of the source steps — those with no declared inputs. Their
    /// real input is outside the graph (a remote service for a
    /// download, a directory named by `common.input_path` for a
    /// file-backed source), so the scheduler cannot version it and
    /// always runs them. These are the valid targets for the runner's
    /// subset-sync mode.
    pub fn fringe_ids(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.inputs.is_empty())
            .map(|s| s.id.as_str())
            .collect()
    }

    pub fn build(steps: Vec<StepSpec>) -> Result<Graph> {
        let mut by_id: HashMap<StepId, usize> = HashMap::new();
        for (i, s) in steps.iter().enumerate() {
            if by_id.insert(s.id.clone(), i).is_some() {
                // Also the single-writer check: a step's id is the tree
                // it writes, so two steps writing one tree is two steps
                // sharing an id.
                bail!("duplicate step id {:?}", s.id);
            }
        }

        let n = steps.len();
        let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut dependents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut resolved_inputs: Vec<Vec<ArtifactPath>> = vec![Vec::new(); n];

        for (bi, b) in steps.iter().enumerate() {
            for input in &b.inputs {
                let Some(&ai) = by_id.get(input.as_str()) else {
                    bail!(
                        "step {:?}: input {input:?} names no declared step. An input is a \
                         step id, not a path on disk — a directory you staged by hand is \
                         named by that step's `params.common.input_path` instead.",
                        b.id
                    );
                };
                if ai == bi {
                    bail!(
                        "step {:?} names itself as an input; a step cannot consume what it \
                         produces — split it into two steps",
                        b.id
                    );
                }
                deps[bi].insert(ai);
                dependents[ai].insert(bi);
                resolved_inputs[bi].push(input.clone());
            }
        }

        // Kahn's algorithm; leftover nodes → cycle.
        let mut indeg: Vec<usize> = deps.iter().map(|d| d.len()).collect();
        let mut ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut topo = Vec::with_capacity(n);
        while let Some(i) = ready.pop() {
            topo.push(i);
            for &j in &dependents[i] {
                indeg[j] -= 1;
                if indeg[j] == 0 {
                    ready.push(j);
                }
            }
        }
        if topo.len() != n {
            let stuck: Vec<&str> = (0..n)
                .filter(|&i| indeg[i] > 0)
                .map(|i| steps[i].id.as_str())
                .collect();
            bail!("dependency cycle among steps: {}", stuck.join(", "));
        }

        let fingerprints = steps.iter().map(fingerprint_of).collect();

        Ok(Graph {
            by_id,
            deps,
            dependents,
            resolved_inputs,
            topo,
            fingerprints,
            steps,
        })
    }
}

fn fingerprint_of(spec: &StepSpec) -> String {
    blake3::hash(spec.fingerprint_material().as_bytes())
        .to_hex()
        .to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::StepRun;

    fn noop_run() -> StepRun {
        StepRun::in_process(|_ctx| async { Ok(crate::step::StepOutcome::default()) })
    }

    fn idx_in(g: &Graph, id: &str) -> usize {
        *g.by_id
            .get(id)
            .unwrap_or_else(|| panic!("no step {id:?}; have {:?}", g.by_id.keys()))
    }

    /// A step is its id plus the ids it reads. There is no `outputs`
    /// argument because there is nothing to say: the step writes the
    /// tree its id names.
    fn spec(id: &str, inputs: &[&str]) -> StepSpec {
        let mut s = StepSpec::new(id, noop_run());
        for i in inputs {
            s = s.input(i);
        }
        s
    }

    #[test]
    fn edges_are_the_declared_inputs() {
        let g = Graph::build(vec![
            spec("slack/raw", &[]),
            spec("slack/rendered_md", &["slack/raw"]),
            spec("email/raw", &[]),
            spec("email/rendered_md", &["email/raw"]),
            spec(
                "unified_index/grid",
                &["slack/rendered_md", "email/rendered_md"],
            ),
        ])
        .unwrap();

        assert_eq!(
            g.deps[idx_in(&g, "slack/rendered_md")],
            BTreeSet::from([idx_in(&g, "slack/raw")])
        );
        assert_eq!(
            g.deps[idx_in(&g, "unified_index/grid")],
            BTreeSet::from([
                idx_in(&g, "slack/rendered_md"),
                idx_in(&g, "email/rendered_md")
            ])
        );
        assert_eq!(
            g.dependents[idx_in(&g, "slack/raw")],
            BTreeSet::from([idx_in(&g, "slack/rendered_md")])
        );

        // Dependencies come before dependents.
        let pos: HashMap<usize, usize> = g.topo.iter().enumerate().map(|(p, &i)| (i, p)).collect();
        assert!(pos[&idx_in(&g, "slack/raw")] < pos[&idx_in(&g, "slack/rendered_md")]);
        assert!(pos[&idx_in(&g, "slack/rendered_md")] < pos[&idx_in(&g, "unified_index/grid")]);
    }

    /// The fringe is what `--sync` may target: steps with no declared
    /// input, whose real input is outside the graph.
    #[test]
    fn fringe_is_the_inputless_steps() {
        let g = Graph::build(vec![
            spec("slack/raw", &[]),
            spec("slack/rendered_md", &["slack/raw"]),
            spec("pdfs/raw", &[]),
        ])
        .unwrap();
        let mut fringe = g.fringe_ids();
        fringe.sort();
        assert_eq!(fringe, vec!["pdfs/raw", "slack/raw"]);
    }

    /// The message has to point somewhere useful: the shape it catches
    /// is a config that names a directory instead of a step.
    #[test]
    fn an_input_naming_no_step_is_an_error() {
        let err = Graph::build(vec![spec("slack/rendered_md", &["slack/raw"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no declared step"), "{err}");
        assert!(err.contains("input_path"), "{err}");
    }

    /// Single-writer, which used to be its own pass over every pair of
    /// output trees, is now the id-uniqueness check: a step's id *is*
    /// the tree it writes.
    #[test]
    fn two_steps_writing_one_tree_is_a_duplicate_id() {
        let err = Graph::build(vec![spec("slack/raw", &[]), spec("slack/raw", &[])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate step id"), "{err}");
    }

    #[test]
    fn a_step_may_not_consume_its_own_output() {
        let err = Graph::build(vec![spec("slack/raw", &["slack/raw"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot consume what it produces"), "{err}");
    }

    #[test]
    fn cycles_are_detected() {
        let err = Graph::build(vec![spec("a", &["b"]), spec("b", &["a"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("dependency cycle"), "{err}");
    }

    /// Nesting is not a relationship. `work-slack/raw` sitting under the
    /// same stem as `work-slack/rendered_md` creates no edge — only a
    /// declared input does. The stem is a display convenience.
    #[test]
    fn a_shared_stem_is_not_an_edge() {
        let g = Graph::build(vec![
            spec("work-slack/raw", &[]),
            spec("work-slack/rendered_md", &[]),
        ])
        .unwrap();
        assert!(g.deps[idx_in(&g, "work-slack/rendered_md")].is_empty());
        assert_eq!(g.fringe_ids().len(), 2);
    }
}
