import { describe, expect, it } from "vitest";
import type { DagResponse, DagStep } from "@/api";
import { dagShape } from "./dagShape";

const step = (id: string, deps: string[] = []): DagStep => ({
  id,
  command: "datalib-step",
  inputs: deps,
  outputs: [id],
  deps,
  last_run: null,
  current_state: null,
  progress: null,
});

const dag = (steps: DagStep[], ok = true): DagResponse => ({
  ok,
  error: ok ? null : "a cycle",
  steps,
  run: null,
});

describe("the DAG card's shape", () => {
  /// The regression: every `dag` frame rebuilt the card, losing its
  /// scroll and the hover, though a run only moves the colours.
  it("holds while a run moves the states and the progress", () => {
    const before = dag([step("a/ingest"), step("a/render", ["a/ingest"])]);
    const after = dag([
      { ...step("a/ingest"), current_state: "running", progress: null },
      { ...step("a/render", ["a/ingest"]), current_state: "succeeded" },
    ]);
    expect(dagShape(after)).toBe(dagShape(before));
  });

  it("moves with a new step, a new edge, or an error", () => {
    const base = dag([step("a/ingest"), step("a/render", ["a/ingest"])]);
    expect(dagShape(dag([...base.steps, step("b/ingest")]))).not.toBe(dagShape(base));
    expect(dagShape(dag([step("a/ingest"), step("a/render")]))).not.toBe(dagShape(base));
    expect(dagShape(dag(base.steps, false))).not.toBe(dagShape(base));
  });
});
