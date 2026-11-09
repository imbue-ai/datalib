// What the toolbar's pill says about a pass over the steps: the launch's
// migrate pass (dag/README.md §"Upgrading a root") or freeing disk space
// (§"Resets and purges"). Each becomes one `PassView`, which
// `PassIndicator.vue` draws the same way whichever it is.

import type { FreeSpace, PassRow, PassState, Upgrade } from "@/api";
import { formatBytes } from "@/config/bytes";

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

const WORST: PassState[] = ["failed", "running", "waiting", "done"];

/// One row per source, in the order the pass first reaches it: the least
/// finished of its steps' states, the first error among them, and the
/// bytes its stores gave back (null when none reported any).
export type SourceRow = StepSource & {
  state: PassState;
  error: string | null;
  freed: number | null;
};

export function sourceRows(steps: PassRow[]): SourceRow[] {
  const rows = new Map<string, SourceRow>();
  for (const s of steps) {
    const src = stepSource(s.step);
    const row = rows.get(src.group) ?? { ...src, state: "done", error: null, freed: null };
    if (WORST.indexOf(s.state) < WORST.indexOf(row.state)) row.state = s.state;
    row.error ??= s.error;
    if (s.store_bytes) row.freed = (row.freed ?? 0) + freedBy(s);
    rows.set(src.group, row);
  }
  return [...rows.values()];
}

function freedBy(s: PassRow): number {
  return s.store_bytes ? Math.max(0, s.store_bytes.before - s.store_bytes.after) : 0;
}

export function freedBytes(steps: PassRow[]): number {
  return steps.reduce((sum, s) => sum + freedBy(s), 0);
}

/// The sources a re-render covers, each once, in the order given.
export function rerenderSources(upgrade: Upgrade): StepSource[] {
  const groups = new Map(
    upgrade.rerender.map((step) => [stepSource(step).group, stepSource(step)]),
  );
  return [...groups.values()];
}

/// One row of the pill's list: the source, and what to say of it.
export type PassViewRow = SourceRow & { said: string };

/// What the toolbar draws for one pass: amber while it runs, red once it
/// is over with a source it could not do, green for a finished pass worth
/// reporting; `null` when there is nothing to say.
export type PassView = {
  tone: "updating" | "failed" | "done";
  label: string;
  heading: string;
  blurb: string;
  rows: PassViewRow[];
  running: boolean;
};

function finished(rows: SourceRow[]): number {
  return rows.filter((r) => r.state === "done" || r.state === "failed").length;
}

function sources(n: number): string {
  return `${n} ${n === 1 ? "source" : "sources"}`;
}

const UPGRADE_SAID: Record<PassState, string> = {
  waiting: "Waiting",
  running: "Updating…",
  done: "Done",
  failed: "Could not update",
};

export function upgradeView(upgrade: Upgrade, dismissed: boolean): PassView | null {
  const all = sourceRows(upgrade.steps);
  const said = (r: SourceRow): PassViewRow => ({ ...r, said: UPGRADE_SAID[r.state] });
  if (upgrade.migrating) {
    return {
      tone: "updating",
      label: `Updating data · ${finished(all)} of ${all.length}`,
      heading: "Updating your data for this version",
      blurb:
        "Each source is bringing what it stored up to date. Search and reading work as usual; " +
        "syncing waits until this is done. Nothing is downloaded while it runs.",
      rows: all.map(said),
      running: true,
    };
  }
  const failed = all.filter((r) => r.state === "failed");
  if (failed.length === 0 || dismissed) return null;
  const ok = all.length - failed.length;
  return {
    tone: "failed",
    label: `${sources(failed.length)} could not be updated`,
    heading: "Your data is updated for this version",
    blurb:
      `${ok} of ${all.length} sources are up to date and can sync again. ` +
      `${failed.length === 1 ? "One" : failed.length} could not be updated; ` +
      "the Sources card has the log.",
    rows: failed.map(said),
    running: false,
  };
}

const FREE_SAID: Record<PassState, string> = {
  waiting: "Waiting",
  running: "Freeing…",
  done: "Done",
  failed: "Could not free",
};

export function freeSpaceView(pass: FreeSpace | null, dismissed: boolean): PassView | null {
  if (!pass) return null;
  const all = sourceRows(pass.steps);
  const freed = freedBytes(pass.steps);
  const rows = all.map((r): PassViewRow => ({
    ...r,
    said:
      r.state === "done" && r.freed !== null
        ? `${FREE_SAID.done} · ${formatBytes(r.freed)}`
        : FREE_SAID[r.state],
  }));
  const what =
    pass.kind === "compact"
      ? "Each store is rewritten without what no version uses, and each source's " +
        "rendered documents keep only their latest version."
      : "Each store is rewritten without what no version uses; every version stays readable.";
  if (pass.running) {
    const counted = all.length > 0 ? ` · ${finished(all)} of ${all.length}` : "";
    const sofar = freed > 0 ? ` · ${formatBytes(freed)} freed` : "";
    return {
      tone: "updating",
      label: `Freeing space${counted}${sofar}`,
      heading: "Freeing up disk space",
      blurb:
        `${what} Search and reading work as usual; a source's syncs wait while its ` +
        "stores are rewritten.",
      rows,
      running: true,
    };
  }
  if (dismissed) return null;
  const failed = all.filter((r) => r.state === "failed").length;
  if (pass.error || failed > 0) {
    return {
      tone: "failed",
      label:
        failed > 0 ? `${sources(failed)} could not free space` : "Could not free up disk space",
      heading: "Freeing up disk space did not finish",
      blurb: `${formatBytes(freed)} was given back. ${pass.error ?? ""}`.trim(),
      rows,
      running: false,
    };
  }
  return {
    tone: "done",
    label: `Freed ${formatBytes(freed)}`,
    heading: "Disk space freed",
    blurb: `${formatBytes(freed)} given back across ${sources(all.length)}.`,
    rows,
    running: false,
  };
}
