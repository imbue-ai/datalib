// What of the pipeline DAG decides how its card is drawn, apart from
// the colours: the steps, their edges and what each hover says about
// them. A `dag` frame arrives on every state change of a run, and one
// whose shape is unchanged only recolours the nodes it already has.
import type { DagResponse, DagStep } from "@/api";

export function dagShape(dag: DagResponse): string {
  return JSON.stringify({
    error: dag.ok ? null : (dag.error ?? "unknown error"),
    steps: dag.steps.map((s) => [s.id, s.deps, s.command, s.inputs, s.outputs]),
  });
}

/// A node's hover: what the step runs, reads and writes, and its state.
export function nodeTitle(step: DagStep, state: string): string {
  return [
    step.id,
    `runs: ${step.command}`,
    step.inputs.length ? `reads: ${step.inputs.join(", ")}` : "reads: (nothing — download step)",
    `writes: ${step.outputs.join(", ")}`,
    state !== "todo" ? `state: ${state}` : "",
  ]
    .filter(Boolean)
    .join("\n");
}
