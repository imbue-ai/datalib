//! Edge derivation and graph validation.
//!
//! The DAG is computed, not declared: step A → step B iff some output
//! of A overlaps some input pattern of B. Validation enforces the
//! invariants the scheduler leans on:
//!
//! * outputs are concrete and non-overlapping across steps (single
//!   writer per artifact tree);
//! * an input pattern that matches no step's output must be a concrete
//!   path — an artifact staged by the user (a wildcard that matches
//!   nothing is almost certainly a typo, and an external wildcard
//!   would make "what are my inputs?" depend on whatever happens to be
//!   on disk);
//! * no cycles.
//!
//! A producer-less input does not stay producer-less: `build`
//! synthesizes a source step that hashes the path and reports the hash
//! as its output version (see [`STAGED_STEP_PREFIX`]). After that every
//! input in the graph has exactly one writer, so the scheduler needs no
//! rule about "external" artifacts at all — they are ordinary outputs
//! of ordinary steps, and `--sync` can name them like any other
//! source.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{bail, Result};

use crate::artifact::ArtifactPat;
use crate::step::{
    ArtifactState, FailureKind, StepCtx, StepError, StepId, StepOutcome, StepRun, StepSpec,
};

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
    /// A topological order (dependencies before dependents).
    pub topo: Vec<usize>,
    /// step idx → hash of everything about the step that is not its
    /// inputs (see [`StepSpec::fingerprint_material`]). A step whose
    /// fingerprint differs from the one recorded at its last success is
    /// stale, which is how a config edit takes effect.
    pub fingerprints: Vec<String>,
}

/// Id prefix of the source steps `build` synthesizes for inputs that no
/// declared step writes. The suffix is the artifact path, so the id is
/// stable across runs and readable in the UI's task list.
pub const STAGED_STEP_PREFIX: &str = "staged:";

impl Graph {
    /// Ids of the source steps — those with no declared inputs. Their
    /// real input is outside the graph (a remote service for a
    /// download, a staged directory for a synthesized `staged:` step),
    /// so the scheduler cannot version it and always runs them. These
    /// are the valid targets for the runner's subset-sync mode.
    pub fn fringe_ids(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.inputs.is_empty())
            .map(|s| s.id.as_str())
            .collect()
    }

    pub fn build(steps: Vec<StepSpec>) -> Result<Graph> {
        let steps = synthesize_staged_sources(steps)?;

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
                        // A wildcard that matches nothing resolves to
                        // the empty set (no edge, no external). This
                        // is deliberate: a starter config carries the
                        // shared fan-in steps (`index`, `qmd` over
                        // `**/rendered_md`) before any source exists,
                        // and must still load and run.
                        continue;
                    }
                    // Only reachable when a step declares the same
                    // path as both an input and an output: the
                    // synthesis pass above leaves it alone (the step
                    // does write it), and a step's own outputs never
                    // satisfy its own inputs. No edge, no producer.
                    resolved_inputs[bi].insert(pat.as_str().to_string(), pat.clone());
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

        let fingerprints = steps.iter().map(fingerprint_of).collect();

        Ok(Graph {
            by_id,
            deps,
            dependents,
            resolved_inputs: resolved_inputs
                .into_iter()
                .map(|m| m.into_values().collect())
                .collect(),
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

/// Give every producer-less concrete input a producer.
///
/// A path the user stages by hand (a Takeout export, a Signal backup)
/// has no step writing it, so nothing can report when it changes. Rather
/// than teach the scheduler to hash such paths itself — a rule that then
/// has to be excepted from everywhere else — synthesize a source step
/// that does exactly that and reports the hash as its output version.
/// Downstream is then ordinary change propagation.
fn synthesize_staged_sources(mut steps: Vec<StepSpec>) -> Result<Vec<StepSpec>> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for b in &steps {
        for pat in &b.inputs {
            if !pat.is_concrete() {
                // A wildcard that matches nothing resolves to the empty
                // set; staging a path for it would be a guess.
                continue;
            }
            let has_producer = steps
                .iter()
                .any(|a| a.outputs.iter().any(|out| pat.overlaps(out)));
            if !has_producer {
                wanted.insert(pat.as_str().to_string());
            }
        }
    }
    for path in wanted {
        let id = format!("{STAGED_STEP_PREFIX}{path}");
        if steps.iter().any(|s| s.id == id) {
            bail!("step id {id:?} collides with a synthesized staged-input step");
        }
        let rel = path.clone();
        let spec = StepSpec::new(
            id,
            StepRun::in_process(move |ctx: StepCtx| {
                let rel = rel.clone();
                async move {
                    let version = crate::version::tree_version(&ctx.path_str(&rel))
                        .map_err(|e| StepError::new(FailureKind::Data, e))?;
                    let pat = ArtifactPat::parse(&rel).expect("validated at graph build");
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, version)],
                    })
                }
            }),
        )
        .output(&path);
        steps.push(spec);
    }
    Ok(steps)
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
        // Every input here has a real producer, so nothing was staged.
        assert!(!g.steps.iter().any(|s| s.id.starts_with(STAGED_STEP_PREFIX)));

        // Topo: every dep precedes its dependent.
        let pos: HashMap<usize, usize> = g.topo.iter().enumerate().map(|(p, &i)| (i, p)).collect();
        for (i, ds) in g.deps.iter().enumerate() {
            for d in ds {
                assert!(pos[d] < pos[&i]);
            }
        }
    }

    #[test]
    fn concrete_unmatched_input_gets_a_synthesized_source_step() {
        let g = Graph::build(vec![spec(
            "takeout.render",
            &["google_takeout/staged_zip"],
            &["google_takeout/rendered_md"],
        )])
        .unwrap();
        // The staged path is no longer producer-less: a source step
        // writes it, and the render depends on that step.
        let staged = idx_in(&g, "staged:google_takeout/staged_zip");
        let render = idx_in(&g, "takeout.render");
        assert!(g.steps[staged].inputs.is_empty(), "staged step is a source");
        assert_eq!(
            g.steps[staged].outputs[0].as_str(),
            "google_takeout/staged_zip"
        );
        assert_eq!(g.deps[render], BTreeSet::from([staged]));
        assert!(g.fringe_ids().contains(&"staged:google_takeout/staged_zip"));
    }

    /// The staged step reports the path's content hash, so a change to
    /// the staged tree shows up as an ordinary version change.
    #[tokio::test]
    async fn synthesized_staged_step_reports_the_path_hash() {
        let td = tempfile::tempdir().unwrap();
        let staged_dir = td.path().join("google_takeout/staged_zip");
        std::fs::create_dir_all(&staged_dir).unwrap();
        std::fs::write(staged_dir.join("a.json"), "v1").unwrap();

        let g = Graph::build(vec![spec(
            "takeout.render",
            &["google_takeout/staged_zip"],
            &["google_takeout/rendered_md"],
        )])
        .unwrap();
        let staged = &g.steps[idx_in(&g, "staged:google_takeout/staged_zip")];
        let StepRun::InProcess(f) = &staged.run else {
            panic!("staged step must be in-process")
        };
        let ctx = StepCtx {
            step_id: staged.id.clone(),
            data_root: td.path().to_path_buf(),
            inputs: vec![],
            changed_inputs: vec![],
            progress: crate::events::StepProgress::new(
                staged.id.clone(),
                std::sync::Arc::new(crate::events::NoopSink),
            ),
        };
        let out = f(ctx.clone()).await.unwrap();
        let v1 = out.outputs[0].version.clone();
        assert_eq!(out.outputs[0].path.as_str(), "google_takeout/staged_zip");
        assert_ne!(v1, crate::version::ABSENT);

        std::fs::write(staged_dir.join("a.json"), "v2").unwrap();
        let out2 = f(ctx).await.unwrap();
        assert_ne!(
            out2.outputs[0].version, v1,
            "content change moves the version"
        );
    }

    #[test]
    fn wildcard_unmatched_input_resolves_empty() {
        // A starter config declares the fan-in steps before any source
        // exists; the wildcard just resolves to nothing.
        let g = Graph::build(vec![spec("index", &["**/rendered_md"], &["system/x"])]).unwrap();
        assert!(g.deps[0].is_empty());
        assert!(g.resolved_inputs[0].is_empty());
        // A wildcard matching nothing is not a staged path — we don't
        // know what would satisfy it.
        assert!(!g.steps.iter().any(|s| s.id.starts_with(STAGED_STEP_PREFIX)));
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
