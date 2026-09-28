// The Manage screen's right-click menu: what it offers for the rows it
// targets, and why an entry is greyed out. Lightroom semantics — the
// targets are the selection when the clicked row is part of it, and
// the clicked row alone when it is not, with the selection left as it
// was either way — are the caller's; this only sees the targets that
// came out of that.
//
// An entry for another kind of row is left out: Rename on a step,
// Compare on the index, anything one-row on a selection. An entry for
// this kind of row that cannot run right now — a sync in the way,
// nothing on disk yet, a reason the row carries — is shown disabled,
// with the reason as its tooltip. On a selection that mixes kinds, an
// entry some of the rows take is disabled, naming a row that does not.

export type MenuKind = "group" | "step" | "applet" | "system";

/// The slice of a Manage row the menu reads.
export type MenuTarget = {
  id: string;
  name: string;
  kind: MenuKind;
  /// The source type, or null for the index group and its steps.
  type: string | null;
  /// The step's function (`ingest`, `qmd_aggregator`, …); null off a step.
  func: string | null;
  runBlocked: string | null;
  editBlocked: string | null;
  revealBlocked: string | null;
  browseBlocked: string | null;
  /// Browse opens the step's raw store rather than the grid: a download
  /// step with one, in the desktop app.
  rawStore: boolean;
  /// The open requests this row has work left in; non-empty is the
  /// state in which Sync reads as Stop.
  stopRequestIds: string[];
  /// Who turned it off; for a group, who turned off every step under it.
  turnedOffBy: string | null;
  /// For a group, the step whose status it shows; the log to open.
  statusFrom: string | null;
  revealPath: string | null;
};

export const RAW_STORE_BROWSE_LABEL = "Browse the downloaded tables";

/// The Browse entry's name — shared with the Actions cell's button, so
/// the two never say different things.
export function browseLabel(t: Pick<MenuTarget, "kind" | "type" | "rawStore">): string {
  if (t.rawStore) return RAW_STORE_BROWSE_LABEL;
  if (t.kind === "system") return "Browse the log";
  return t.kind === "group" && !t.type ? "Browse every source" : "Browse this data";
}

/// Why an entry that edits the config does not apply, or null when the
/// row is a config entry.
const NOT_IN_CONFIG = "Not a config entry";

function notInConfig(t: MenuTarget): string | null {
  return t.kind === "system" ? NOT_IN_CONFIG : null;
}

export type MenuAction =
  | "browse"
  | "sync"
  | "stop"
  | "turn_off"
  | "turn_on"
  | "edit"
  | "compare"
  | "rename"
  | "copy_id"
  | "copy_path"
  | "log"
  | "history"
  | "reveal"
  | "reset"
  | "remove";

export type MenuEntry =
  | { separator: true }
  | {
      separator?: false;
      action: MenuAction;
      name: string;
      /// Why this entry cannot run for the targets, or null when it can.
      /// Shown as the tooltip of the disabled entry.
      disabled: string | null;
    };

export type MenuOptions = {
  /// Whether this host can show a path in its file manager at all; the
  /// entry is absent in a plain browser, as the button is.
  canReveal: boolean;
  revealLabel: string;
};

/// A tree that keeps no doltlite store, or null when it keeps one.
/// Why "Compare…" does not apply: a comparison is of a source — a group
/// with a type — that is not itself one (`docs/dev/plans/completed/diff_renderer.md`).
export function notComparableReason(t: MenuTarget): string | null {
  if (t.kind === "system") return NOT_IN_CONFIG;
  if (t.kind !== "group") return "Compare a source, not a step under it";
  if (!t.type) return "The index mirrors nothing to compare";
  if (t.type === "diff") return "Already a comparison — make another from its source";
  return null;
}

export function noStoreReason(t: MenuTarget): string | null {
  if (t.kind === "applet") return "An applet writes no store";
  if (t.kind === "system") return "The run log is plain SQLite, with no commit history";
  if (t.func === "qmd_aggregator" || t.func === "keyword_index" || t.func === "embed") {
    return "The QMD index keeps no doltlite store";
  }
  if (t.func === "embedding_map") return "The embedding map keeps no doltlite store";
  return null;
}

/// Why "Reset…" is not for this row: a reset empties
/// what a source downloaded or rendered, and what reads it follows — so
/// the index, which follows every source, is not reset by hand, and an
/// applet writes nothing. The embedding map is the exception: each run
/// starts from the last map, and a reset is how a person asks for one
/// laid out afresh.
export function notResettableReason(t: MenuTarget): string | null {
  if (t.kind === "system") return NOT_IN_CONFIG;
  if (t.kind === "applet") return "An applet writes no store";
  if (t.kind === "step" && t.func === "embedding_map") return null;
  if (
    !t.type ||
    t.func === "grid_index" ||
    t.func === "qmd_aggregator" ||
    t.func === "keyword_index" ||
    t.func === "embed"
  ) {
    return "Reset a source; the index follows it";
  }
  return null;
}

/// Why "Turn off" does not apply: only the loop's steps are scheduled.
export function notSwitchableReason(t: MenuTarget): string | null {
  if (t.kind === "system") return NOT_IN_CONFIG;
  if (t.kind === "applet") return "An applet is not scheduled";
  return null;
}

function firstBlocked(
  targets: MenuTarget[],
  pick: (t: MenuTarget) => string | null,
): string | null {
  for (const t of targets) {
    const why = pick(t);
    if (why) return targets.length === 1 ? why : `${t.name}: ${why}`;
  }
  return null;
}

/// An entry that no target is the kind of row for.
const ABSENT = Symbol("absent");
type Verdict = string | null | typeof ABSENT;

/// A rule about which kind of row an entry is for, over the targets.
function forKind(targets: MenuTarget[], pick: (t: MenuTarget) => string | null): Verdict {
  return targets.every((t) => pick(t) !== null) ? ABSENT : firstBlocked(targets, pick);
}

function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

/// No separator first, last, or twice in a row once entries are left out.
function tidySeparators(entries: MenuEntry[]): MenuEntry[] {
  const out: MenuEntry[] = [];
  for (const e of entries) {
    if (e.separator && (out.length === 0 || out[out.length - 1].separator)) continue;
    out.push(e);
  }
  while (out.length > 0 && out[out.length - 1].separator) out.pop();
  return out;
}

export function rowMenu(targets: MenuTarget[], opts: MenuOptions): MenuEntry[] {
  if (targets.length === 0) return [];
  const one = targets.length === 1;
  const only = targets[0];
  const oneRow: Verdict = one ? null : ABSENT;
  const busy = firstBlocked(targets, (t) =>
    t.stopRequestIds.length > 0 ? "Busy — stop the sync first" : null,
  );
  const entries: MenuEntry[] = [];
  const add = (action: MenuAction, name: string, disabled: Verdict) => {
    if (disabled !== ABSENT) entries.push({ action, name, disabled });
  };
  const separator = () => entries.push({ separator: true });

  add("browse", browseLabel(only), oneRow ?? only.browseBlocked);
  const claimed = targets.filter((t) => t.stopRequestIds.length > 0).length;
  if (claimed === targets.length) {
    add("stop", one ? "Stop the sync" : `Stop ${plural(targets.length, "sync")}`, null);
  } else {
    add(
      "sync",
      "Sync now",
      forKind(targets, notInConfig) ??
        (claimed > 0
          ? `${plural(claimed, "row")} of these already syncing — stop it first, or pick the others`
          : firstBlocked(targets, (t) => t.runBlocked)),
    );
  }
  const off = targets.filter((t) => t.turnedOffBy).length;
  add(
    off === targets.length ? "turn_on" : "turn_off",
    off === targets.length ? "Turn on" : "Turn off",
    forKind(targets, notSwitchableReason),
  );
  separator();

  add("edit", "Edit settings…", oneRow ?? forKind(targets, notInConfig) ?? only.editBlocked);
  add(
    "rename",
    "Rename…",
    oneRow ??
      forKind(
        targets,
        (t) => notInConfig(t) ?? (t.kind !== "group" ? "Only a group has a name" : null),
      ),
  );
  separator();

  add("history", "Show commit history", forKind(targets, noStoreReason));
  add("compare", "Compare two versions…", oneRow ?? forKind(targets, notComparableReason));
  add(
    "log",
    "Show step log",
    oneRow ??
      (only.kind === "applet" || only.kind === "system"
        ? ABSENT
        : only.kind === "group" && !only.statusFrom
          ? "No step under this group has run yet"
          : null),
  );
  separator();

  if (opts.canReveal) {
    add(
      "reveal",
      opts.revealLabel,
      firstBlocked(targets, (t) => t.revealBlocked),
    );
  }
  add(
    "copy_path",
    one ? "Copy path" : `Copy ${plural(targets.length, "path")}`,
    firstBlocked(targets, (t) => (t.revealPath ? null : "Nothing on disk yet")),
  );
  add("copy_id", one ? "Copy id" : `Copy ${plural(targets.length, "id")}`, null);
  separator();

  add("reset", "Reset…", forKind(targets, notResettableReason) ?? busy);
  add(
    "remove",
    one
      ? only.kind === "group"
        ? "Remove from config, with everything under it"
        : "Remove from config"
      : `Remove ${targets.length} entries from config`,
    forKind(targets, notInConfig),
  );
  return tidySeparators(entries);
}
