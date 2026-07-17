//! Edge derivation and graph validation.
//!
//! The DAG is computed, not declared: step A → step B iff some output
//! of A overlaps some input pattern of B. Validation enforces the
//! invariants the scheduler leans on:
//!
//! * outputs are concrete and non-overlapping across steps (single
//!   writer per artifact tree);
//! * an input pattern that matches no step's output must be a concrete
//!   path — an "external" artifact staged by the user (a wildcard that
//!   matches nothing is almost certainly a typo, and an external
//!   wildcard would make "what are my inputs?" depend on whatever
//!   happens to be on disk);
//! * no cycles.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{bail, Result};

use crate::artifact::ArtifactPat;
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
    /// step idx → the concrete input artifacts its patterns resolved
    /// to: producer outputs plus external (producer-less) paths.
    pub resolved_inputs: Vec<Vec<ArtifactPat>>,
    /// Concrete input artifacts no step produces, per step. Subset of
    /// `resolved_inputs`; the scheduler content-hashes these itself.
    pub external_inputs: Vec<Vec<ArtifactPat>>,
    /// A topological order (dependencies before dependents).
    pub topo: Vec<usize>,
}

impl Graph {
    /// Ids of the fringe steps — those with no declared inputs (the
    /// download steps, whose real input is a remote service). These
    /// are the valid targets for the runner's subset-sync mode.
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
                bail!("duplicate step id {:?}", s.id);
            }
            for out in &s.outputs {
                if !out.is_concrete() {
                    bail!(
                        "step {:?}: output {out} contains wildcards; outputs must be concrete",
                        s.id
                    );
                }
            }
        }

        // Single-writer check: no two steps' output trees may
        // intersect (including one step's own outputs against another's
        // — a shared tree means "who owns this?" is ambiguous).
        for (i, a) in steps.iter().enumerate() {
            for b in steps.iter().skip(i + 1) {
                for oa in &a.outputs {
                    for ob in &b.outputs {
                        if oa.conflicts_with(ob) {
                            bail!(
                                "steps {:?} and {:?} both write into {oa} / {ob}; \
                                 every artifact tree has exactly one producer",
                                a.id,
                                b.id
                            );
                        }
                    }
                }
            }
        }

        let n = steps.len();
        let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut dependents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut resolved_inputs: Vec<BTreeMap<String, ArtifactPat>> = vec![BTreeMap::new(); n];
        let mut external_inputs: Vec<Vec<ArtifactPat>> = vec![Vec::new(); n];

        for (bi, b) in steps.iter().enumerate() {
            for pat in &b.inputs {
                let mut matched = false;
                for (ai, a) in steps.iter().enumerate() {
                    if ai == bi {
                        // A step's own outputs never satisfy its
                        // inputs — wildcard inputs would otherwise
                        // self-loop (e.g. the index step under a
                        // `**/rendered_md` input while writing
                        // `system/backend_index`).
                        continue;
                    }
                    for out in &a.outputs {
                        if pat.overlaps(out) {
                            matched = true;
                            deps[bi].insert(ai);
                            dependents[ai].insert(bi);
                            resolved_inputs[bi].insert(out.as_str().to_string(), out.clone());
                        }
                    }
                }
                if !matched {
                    if !pat.is_concrete() {
                        bail!(
                            "step {:?}: wildcard input {pat} matches no step's output; \
                             external inputs must be concrete paths",
                            b.id
                        );
                    }
                    resolved_inputs[bi].insert(pat.as_str().to_string(), pat.clone());
                    external_inputs[bi].push(pat.clone());
                }
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

        Ok(Graph {
            by_id,
            deps,
            dependents,
            resolved_inputs: resolved_inputs
                .into_iter()
                .map(|m| m.into_values().collect())
                .collect(),
            external_inputs,
            topo,
            steps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::StepRun;

    fn noop_run() -> StepRun {
        StepRun::in_process(|_ctx| async { Ok(crate::step::StepOutcome::default()) })
    }

    fn spec(id: &str, inputs: &[&str], outputs: &[&str]) -> StepSpec {
        let mut s = StepSpec::new(id, noop_run());
        for i in inputs {
            s = s.input(i);
        }
        for o in outputs {
            s = s.output(o);
        }
        s
    }

    #[test]
    fn derives_chain_and_wildcard_fan_in() {
        let g = Graph::build(vec![
            spec("slack.download", &[], &["slack/raw"]),
            spec("slack.render", &["slack/raw"], &["slack/rendered_md"]),
            spec("email.download", &[], &["email/raw"]),
            spec("email.render", &["email/raw"], &["email/rendered_md"]),
            spec("index", &["**/rendered_md"], &["system/backend_index"]),
        ])
        .unwrap();

        let idx = |id: &str| g.by_id[id];
        assert_eq!(
            g.deps[idx("slack.render")],
            BTreeSet::from([idx("slack.download")])
        );
        assert_eq!(
            g.deps[idx("index")],
            BTreeSet::from([idx("slack.render"), idx("email.render")])
        );
        // The wildcard resolved to the two concrete producer outputs.
        let inputs: Vec<&str> = g.resolved_inputs[idx("index")]
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(inputs, vec!["email/rendered_md", "slack/rendered_md"]);
        assert!(g.external_inputs[idx("index")].is_empty());

        // Topo: every dep precedes its dependent.
        let pos: HashMap<usize, usize> = g.topo.iter().enumerate().map(|(p, &i)| (i, p)).collect();
        for (i, ds) in g.deps.iter().enumerate() {
            for d in ds {
                assert!(pos[d] < pos[&i]);
            }
        }
    }

    #[test]
    fn concrete_unmatched_input_is_external() {
        let g = Graph::build(vec![spec(
            "takeout.render",
            &["google_takeout/staged_zip"],
            &["google_takeout/rendered_md"],
        )])
        .unwrap();
        assert_eq!(g.external_inputs[0].len(), 1);
        assert_eq!(
            g.external_inputs[0][0].as_str(),
            "google_takeout/staged_zip"
        );
    }

    #[test]
    fn wildcard_unmatched_input_is_an_error() {
        let err = Graph::build(vec![spec("index", &["**/rendered_md"], &["system/x"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches no step's output"), "{err}");
    }

    #[test]
    fn overlapping_outputs_are_an_error() {
        let err = Graph::build(vec![
            spec("a", &[], &["slack/raw"]),
            spec("b", &[], &["slack/raw/db"]),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one producer"), "{err}");
    }

    #[test]
    fn wildcard_output_is_an_error() {
        let err = Graph::build(vec![spec("a", &[], &["*/raw"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("outputs must be concrete"), "{err}");
    }

    #[test]
    fn cycles_are_detected() {
        let err = Graph::build(vec![spec("a", &["y"], &["x"]), spec("b", &["x"], &["y"])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn own_output_does_not_satisfy_own_input() {
        // `index` writes under system/ while reading `**/rendered_md`;
        // `**` could match its own output tree — make sure that doesn't
        // become a self-edge (which would read as a cycle).
        let g = Graph::build(vec![
            spec("render", &[], &["slack/rendered_md"]),
            spec("index", &["**"], &["system/backend_index"]),
        ])
        .unwrap();
        let idx = |id: &str| g.by_id[id];
        assert_eq!(g.deps[idx("index")], BTreeSet::from([idx("render")]));
    }
}
