// What the launch's upgrade indicator and dialog say about a step
// (docs/dev/plans/upgrade_on_launch.md).

import type { MigrateState, Upgrade } from "@/api";

/// The source a step belongs to, as a person would name it: its group,
/// or the search index for the index's own steps.
export function stepSource(step: string): string {
  const group = step.split("/")[0];
  return group === "unified_index" ? "Search index" : group;
}

const WORST: MigrateState[] = ["failed", "running", "waiting", "done"];

/// One row per source, in the order the pass first reaches it: the least
/// finished of its steps' states, and the first error among them.
export function sourceRows(
  upgrade: Upgrade,
): { source: string; state: MigrateState; error: string | null }[] {
  const rows = new Map<string, { source: string; state: MigrateState; error: string | null }>();
  for (const s of upgrade.steps) {
    const source = stepSource(s.step);
    const row = rows.get(source) ?? { source, state: "done", error: null };
    if (WORST.indexOf(s.state) < WORST.indexOf(row.state)) row.state = s.state;
    row.error ??= s.error;
    rows.set(source, row);
  }
  return [...rows.values()];
}

/// The sources a re-render covers, each once, in the order given.
export function rerenderSources(upgrade: Upgrade): string[] {
  return [...new Set(upgrade.rerender.map(stepSource))];
}

/// The sources with a step the launch could not migrate, each once, with
/// the first reason.
export function failedSources(upgrade: Upgrade): { source: string; error: string }[] {
  return sourceRows(upgrade)
    .filter((r) => r.state === "failed")
    .map((r) => ({ source: r.source, error: r.error ?? "" }));
}

/// The toolbar's upgrade pill: amber while the pass runs, red once it is
/// over with a source it could not update (until dismissed), else none.
export type UpgradePill = { tone: "updating" | "failed"; label: string };

export function upgradePill(upgrade: Upgrade, dismissed: boolean): UpgradePill | null {
  const rows = sourceRows(upgrade);
  if (upgrade.migrating) {
    const done = rows.filter((r) => r.state === "done" || r.state === "failed").length;
    return { tone: "updating", label: `Updating data · ${done} of ${rows.length}` };
  }
  const failed = rows.filter((r) => r.state === "failed").length;
  if (failed === 0 || dismissed) return null;
  return {
    tone: "failed",
    label: `${failed} ${failed === 1 ? "source" : "sources"} could not be updated`,
  };
}
