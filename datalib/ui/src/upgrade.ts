// What the launch's upgrade indicator and dialog say about a step
// (docs/dev/plans/upgrade_on_launch.md).

import type { MigrateState, Upgrade } from "@/api";

const INDEX_GROUP = "unified_document_index";

/// The source a step belongs to: its group, and what to call it until
/// its chip resolves — the search index by that name, a source by its id.
export type StepSource = { group: string; source: string };

export function stepSource(step: string): StepSource {
  const group = step.split("/")[0];
  return { group, source: group === INDEX_GROUP ? "Search index" : group };
}

/// The search index keeps its plain name on a chip; a source shows the
/// name the config gives it.
export function keepsItsName(group: string): boolean {
  return group === INDEX_GROUP;
}

const WORST: MigrateState[] = ["failed", "running", "waiting", "done"];

/// One row per source, in the order the pass first reaches it: the least
/// finished of its steps' states, and the first error among them.
export type SourceRow = StepSource & { state: MigrateState; error: string | null };

export function sourceRows(upgrade: Upgrade): SourceRow[] {
  const rows = new Map<string, SourceRow>();
  for (const s of upgrade.steps) {
    const src = stepSource(s.step);
    const row = rows.get(src.group) ?? { ...src, state: "done", error: null };
    if (WORST.indexOf(s.state) < WORST.indexOf(row.state)) row.state = s.state;
    row.error ??= s.error;
    rows.set(src.group, row);
  }
  return [...rows.values()];
}

/// The sources a re-render covers, each once, in the order given.
export function rerenderSources(upgrade: Upgrade): StepSource[] {
  const groups = new Map(
    upgrade.rerender.map((step) => [stepSource(step).group, stepSource(step)]),
  );
  return [...groups.values()];
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
