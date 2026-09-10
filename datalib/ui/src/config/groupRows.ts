// What a group's row on the Manage screen says about the steps and
// applets filed under it: the order they run in, the status the row
// shows, the instant it calls "last synced", and the steps a sync of the
// group starts at. Pure functions over the config entries and each
// child's own status view, so the rules are testable without a grid.
//
// The rules are the aggregation table in
// docs/dev/plans/groups_and_functions.md. Nothing here does arithmetic
// across children: a group's bytes come from its own measured series,
// and its progress is one segment per child.

import type { EntryKind, StepPhase } from "./sourceSteps";
import type { StatusView } from "./pipelineStatus";
import { compareStamps } from "./timeFormat";

/// The grid's row id for a group. An applet may share its group's id —
/// the `unified_index` applet sits under the `unified_index` group — so
/// a group row needs a key no entry can have.
export function groupRowKey(groupId: string): string {
  return `group:${groupId}`;
}

export type ChildEntry = {
  id: string;
  kind: EntryKind;
  inputs: string[];
};

/// A group's children in pipeline order: a step that reads a sibling
/// comes after it, ties keep config order, and applets — never
/// scheduled — trail the steps.
export function pipelineOrder<T extends ChildEntry>(children: T[]): T[] {
  const steps = children.filter((c) => c.kind === "step");
  const applets = children.filter((c) => c.kind !== "step");
  const siblingIds = new Set(steps.map((s) => s.id));
  const placed: T[] = [];
  const done = new Set<string>();
  while (placed.length < steps.length) {
    const ready = steps.find(
      (s) =>
        !done.has(s.id) && s.inputs.every((input) => !siblingIds.has(input) || done.has(input)),
    );
    // A cycle within a group is a config the loader refuses, but this
    // runs against unsaved text too: fall back to config order rather
    // than spin.
    const next = ready ?? steps.find((s) => !done.has(s.id))!;
    done.add(next.id);
    placed.push(next);
  }
  return [...placed, ...applets];
}

export type ChildStatus = {
  id: string;
  kind: EntryKind;
  status: StatusView;
};

/// The status a group row shows, and which child it is read from.
///
/// Running if any child is running; failed if any child failed;
/// otherwise the last step in pipeline order — the one whose state says
/// how far the group's data got. A group with only applets reads its
/// last applet. `children` must already be in pipeline order.
export function groupStatus(
  children: ChildStatus[],
): { status: StatusView; from: string } | null {
  if (children.length === 0) return null;
  const running = children.find((c) => c.status.key === "running");
  if (running) return read(running);
  const failed = children.find((c) => c.status.key === "failed");
  if (failed) return read(failed);
  const steps = children.filter((c) => c.kind === "step");
  return read(steps[steps.length - 1] ?? children[children.length - 1]);
}

/// The child's view, with the child named in the detail so the group's
/// tooltip says where its word came from.
function read(child: ChildStatus): { status: StatusView; from: string } {
  const { status } = child;
  const detail = status.detail ? `${child.id}: ${status.detail}` : child.id;
  return { status: { ...status, detail }, from: child.id };
}

export type ChildStamp = {
  kind: EntryKind;
  phase: StepPhase;
  at: string | null;
};

/// When a group last synced: its fetch step's instant, else the newest
/// any child reports. The fetch step is what "synced" means for a
/// source, so a render that ran later does not move the group's stamp.
export function groupLastSynced(children: ChildStamp[]): string | null {
  const fetch = children.find((c) => c.kind === "step" && c.phase === "fetch");
  if (fetch) return fetch.at;
  let newest: string | null = null;
  for (const c of children) {
    if (c.at && compareStamps(c.at, newest) > 0) newest = c.at;
  }
  return newest;
}

/// The steps a sync of the group starts at: its steps with no declared
/// inputs, less any the loader dropped. `datalib-dag --sync` takes
/// exactly these, and everything downstream follows.
export function groupSeeds<T extends ChildEntry>(
  children: T[],
  isDropped: (child: T) => boolean,
): string[] {
  return children
    .filter((c) => c.kind === "step" && c.inputs.length === 0 && !isDropped(c))
    .map((c) => c.id);
}
