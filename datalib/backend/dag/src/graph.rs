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
//! A step that breaks one of those is *left out of the graph*, and
//! [`Graph::build_graded`] says which and why (see
//! `crate::diagnostics`). It is not carried in the graph with a failed
//! status, and that is deliberate: the scheduler's invariant is that
//! every artifact in the graph has exactly one producer, so a step
//! whose input names nothing would make the runner invent a version for
//! a tree nobody wrote. Excluding it keeps the invariant and is why
//! none of #209 reached `scheduler.rs`.
//!
//! This module used to derive edges by testing every input pattern
//! against every output path, and to synthesize source steps for
//! producer-less inputs. Both are gone with the pattern machinery they
//! rested on — see `docs/dev/step_identity.md`.

use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Result};

use crate::artifact::ArtifactPath;
use crate::diagnostics::{Diagnostic, EntryRef, Severity};
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

    /// Assemble the graph, strictly: the first problem is an `Err`.
    ///
    /// The strict view of [`Graph::build_graded`]. Kept for callers
    /// that build specs by hand and want a bad one to be a loud failure
    /// rather than a silently smaller graph.
    pub fn build(steps: Vec<StepSpec>) -> Result<Graph> {
        let (graph, diags) = Self::build_graded(steps, &BTreeSet::new());
        if let Some(d) = diags.first() {
            bail!("{}", d.describe());
        }
        Ok(graph)
    }

    /// Assemble what can be assembled, and say what could not.
    ///
    /// Three rules live here, because all three need the full set of
    /// ids and nothing earlier has it:
    ///
    ///   * every input names a declared step;
    ///   * no step consumes its own output;
    ///   * no cycles.
    ///
    /// A step that breaks one is left out of the graph entirely rather
    /// than carried in it with a failed status. That is what keeps the
    /// scheduler's invariant true — every artifact in the graph has
    /// exactly one producer, so the runner never has to invent a
    /// version for a tree nobody wrote — and it is why the scheduler
    /// needed no changes for any of this.
    ///
    /// Dropping cascades, and the diagnostics say so. A step whose
    /// input names a step that was itself dropped is
    /// [`Severity::Blocked`], not `Rejected`: nothing is wrong with it,
    /// and sending the user to its line would send them to the wrong
    /// line. The message names the entry that actually needs the edit.
    ///
    /// `dropped_earlier` is what the *config* pass already threw out
    /// before these specs were built. Without it this pass cannot tell
    /// "you named a step that does not exist" from "the step you named
    /// is broken" for the commonest case of all — a render step whose
    /// fetch step was rejected for a bad key — and would send the user
    /// to fix the wrong entry. Callers building specs by hand pass an
    /// empty set; [`crate::config::check_text`] passes the real one.
    pub fn build_graded(
        steps: Vec<StepSpec>,
        dropped_earlier: &BTreeSet<String>,
    ) -> (Graph, Vec<Diagnostic>) {
        let mut diags = Vec::new();

        // Ids first: everything below indexes by id, so duplicates have
        // to go before anything can be looked up. Config-side loading
        // has normally removed these already; specs built by hand have
        // not, and this is the only place that can tell.
        let mut kept: Vec<StepSpec> = Vec::with_capacity(steps.len());
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for s in steps {
            if !ids.insert(s.id.clone()) {
                diags.push(
                    Diagnostic::new(
                        Severity::Rejected,
                        format!(
                            "duplicate step id {:?}: a step's id is the tree it writes, so \
                             two steps sharing one write the same files",
                            s.id
                        ),
                    )
                    .at_entry(EntryRef::step_id(s.id.clone())),
                );
                continue;
            }
            kept.push(s);
        }

        // Then drop to a fixpoint. One pass is not enough: a step
        // dropped here can be the reason the next one has to go, and
        // that chain can run any length.
        //
        // `dropped` remembers which ids are gone, which is what lets a
        // cascade diagnostic say "the thing you named is broken"
        // instead of "the thing you named does not exist". They send
        // the reader to different lines, so the difference is the whole
        // point of tracking it.
        let mut dropped: BTreeSet<String> = dropped_earlier.clone();
        loop {
            let live: BTreeSet<&str> = kept.iter().map(|s| s.id.as_str()).collect();
            let mut doomed: Option<(usize, Diagnostic)> = None;
            'scan: for (i, s) in kept.iter().enumerate() {
                for input in &s.inputs {
                    let name = input.as_str();
                    if name == s.id {
                        doomed = Some((
                            i,
                            Diagnostic::new(
                                Severity::Rejected,
                                "names itself as an input; a step cannot consume what it \
                                 produces",
                            )
                            .at_entry(EntryRef::step_id(s.id.clone()))
                            .with_help("split it into two steps"),
                        ));
                        break 'scan;
                    }
                    if live.contains(name) {
                        continue;
                    }
                    let d = if dropped.contains(name) {
                        Diagnostic::new(
                            Severity::Blocked,
                            format!(
                                "cannot run: its input {name:?} was itself dropped from this \
                                 config"
                            ),
                        )
                        .at_entry(EntryRef::step_id(s.id.clone()))
                        .with_help(format!(
                            "nothing is wrong with this step — fix {name:?} and this one runs \
                             again"
                        ))
                    } else {
                        Diagnostic::new(
                            Severity::Blocked,
                            format!("input {name:?} names no declared step"),
                        )
                        .at_entry(EntryRef::step_id(s.id.clone()))
                        .with_help(format!(
                            "an input is a step id, not a path on disk — a directory you \
                             staged by hand is named by that step's \
                             `params.common.input_path` instead. Declared steps: {}",
                            id_list(&live, &s.id)
                        ))
                    };
                    doomed = Some((i, d));
                    break 'scan;
                }
            }
            match doomed {
                Some((i, d)) => {
                    dropped.insert(kept[i].id.clone());
                    kept.remove(i);
                    diags.push(d);
                }
                None => break,
            }
        }

        // Cycles. Kahn's algorithm leaves behind every node it could
        // not order — the ring itself *and* everything downstream of it
        // — so the two have to be told apart before either is
        // described, or a step three hops below a cycle gets a message
        // claiming it is in one.
        let (graph, cycle_diags) = Self::assemble(kept);
        diags.extend(cycle_diags);
        (graph, diags)
    }

    /// Index the steps and order them, splitting off anything a cycle
    /// makes unschedulable. Every input is known to resolve by the time
    /// this is called.
    fn assemble(steps: Vec<StepSpec>) -> (Graph, Vec<Diagnostic>) {
        let n = steps.len();
        let mut by_id: HashMap<StepId, usize> = HashMap::new();
        for (i, s) in steps.iter().enumerate() {
            by_id.insert(s.id.clone(), i);
        }

        let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut dependents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut resolved_inputs: Vec<Vec<ArtifactPath>> = vec![Vec::new(); n];

        for (bi, b) in steps.iter().enumerate() {
            for input in &b.inputs {
                // Every input resolves: `build_graded` removed every
                // step for which one didn't.
                let ai = by_id[input.as_str()];
                deps[bi].insert(ai);
                dependents[ai].insert(bi);
                resolved_inputs[bi].push(input.clone());
            }
        }

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
            let stuck: Vec<usize> = (0..n).filter(|&i| indeg[i] > 0).collect();
            let in_cycle: Vec<usize> = stuck
                .iter()
                .copied()
                .filter(|&i| reaches_itself(&dependents, i))
                .collect();
            let ring: Vec<&str> = in_cycle.iter().map(|&i| steps[i].id.as_str()).collect();
            let mut diags = Vec::new();
            for &i in &stuck {
                let d = if in_cycle.contains(&i) {
                    // A cycle has no innocent member and no root to
                    // point at: the edit that breaks it could be any
                    // one of them, so each gets the whole ring.
                    Diagnostic::new(
                        Severity::Blocked,
                        format!("is in a dependency cycle: {}", ring.join(" → ")),
                    )
                    .with_help("remove one of the `inputs` entries that closes the ring")
                } else {
                    Diagnostic::new(
                        Severity::Blocked,
                        format!(
                            "cannot run: it is downstream of a dependency cycle ({})",
                            ring.join(" → ")
                        ),
                    )
                    .with_help("nothing is wrong with this step — break the cycle above it")
                };
                diags.push(d.at_entry(EntryRef::step_id(steps[i].id.clone())));
            }
            let survivors: Vec<StepSpec> = steps
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !stuck.contains(i))
                .map(|(_, s)| s)
                .collect();
            let (graph, more) = Self::assemble(survivors);
            // The survivors are exactly what Kahn *did* order, so they
            // are cycle-free and their inputs all resolved. Assert
            // rather than quietly merge, so a rule that does start
            // firing here is not swallowed.
            debug_assert!(more.is_empty(), "second assemble pass produced {more:?}");
            diags.extend(more);
            return (graph, diags);
        }

        let fingerprints = steps.iter().map(fingerprint_of).collect();
        (
            Graph {
                by_id,
                deps,
                dependents,
                resolved_inputs,
                topo,
                fingerprints,
                steps,
            },
            Vec::new(),
        )
    }
}

/// Whether `start` is reachable from itself along dependent edges —
/// i.e. it is in a cycle, rather than merely downstream of one.
///
/// Kahn's algorithm cannot tell those apart: it leaves behind
/// everything it could not order, ring and tail alike. Telling the user
/// a step three hops below a cycle is *in* the cycle sends them looking
/// for an `inputs` entry that isn't there.
fn reaches_itself(dependents: &[BTreeSet<usize>], start: usize) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(i) = stack.pop() {
        for &j in &dependents[i] {
            if j == start {
                return true;
            }
            if seen.insert(j) {
                stack.push(j);
            }
        }
    }
    false
}

/// The declared step ids, for a diagnostic that has to say what the
/// valid choices were — minus the step doing the asking, which is
/// never the answer and reads as noise in its own error message.
///
/// Capped: a config with sixty steps would push the actual message off
/// the screen, and the point is to jog a memory, not to dump the file
/// back at the reader.
fn id_list(ids: &BTreeSet<&str>, excluding: &str) -> String {
    const MAX: usize = 12;
    let all: Vec<&str> = ids.iter().copied().filter(|id| *id != excluding).collect();
    if all.is_empty() {
        return "(none — this config declares no other steps)".to_string();
    }
    let shown: Vec<&str> = all.iter().copied().take(MAX).collect();
    if all.len() > MAX {
        format!("{}, … (+{} more)", shown.join(", "), all.len() - MAX)
    } else {
        shown.join(", ")
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
