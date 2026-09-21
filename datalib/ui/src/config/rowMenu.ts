// The Manage screen's right-click menu: what it offers for the rows it
// targets, and why an entry is greyed out. Lightroom semantics — the
// targets are the selection when the clicked row is part of it, and
// the clicked row alone when it is not, with the selection left as it
// was either way — are the caller's; this only sees the targets that
// came out of that.
//
// Every entry is always present. One that does not apply is disabled
// with the reason as its tooltip, the same rule the action buttons
// follow, so a person learns what a row *could* do by reading the menu.

export type MenuKind = "group" | "step" | "applet" | "system";

/// The slice of a Manage row the menu reads.
export type MenuTarget = {
  id: string;
  name: string;
  kind: MenuKind;
  /// The source type, or null for the index group and its steps.
  type: string | null;
  /// The step's function (`ingest`, `qmd_index`, …); null off a step.
  func: string | null;
  runBlocked: string | null;
  editBlocked: string | null;
  revealBlocked: string | null;
  browseBlocked: string | null;
  /// Non-null while a job has this row claimed — the state in which
  /// Sync reads as Stop.
  stopJobId: string | null;
  /// For a group, the step whose status it shows; the log to open.
  statusFrom: string | null;
  revealPath: string | null;
};

/// The Browse entry's name — shared with the Actions cell's button, so
/// the two never say different things.
export function browseLabel(t: Pick<MenuTarget, "kind" | "type">): string {
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
  | "edit"
  | "compare"
  | "rename"
  | "copy_id"
  | "copy_path"
  | "log"
  | "history"
  | "reveal"
  | "remove";

export type MenuEntry =
  | { separator: true }
  | {
      separator?: false;
      action: MenuAction;
      name: string;
      /// Why this entry does not apply to the targets, or null when it
      /// does. Shown as the tooltip of the disabled entry.
      disabled: string | null;
    };

export type MenuOptions = {
  /// Which column was under the pointer, for the entries a cell adds.
  column: string;
  /// Whether this host can show a path in its file manager at all; the
  /// entry is absent in a plain browser, as the button is.
  canReveal: boolean;
  revealLabel: string;
};

const ONE_AT_A_TIME = "One row at a time";

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
  if (t.func === "qmd_index") return "The QMD index keeps no doltlite store";
  return null;
}

function firstBlocked(targets: MenuTarget[], pick: (t: MenuTarget) => string | null): string | null {
  for (const t of targets) {
    const why = pick(t);
    if (why) return targets.length === 1 ? why : `${t.name}: ${why}`;
  }
  return null;
}

function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

export function rowMenu(targets: MenuTarget[], opts: MenuOptions): MenuEntry[] {
  if (targets.length === 0) return [];
  const one = targets.length === 1;
  const only = targets[0];
  const entries: MenuEntry[] = [];

  // What the cell under the pointer adds, ahead of what the row offers.
  if (opts.column === "name") {
    entries.push({
      action: "rename",
      name: "Rename…",
      disabled: !one
        ? ONE_AT_A_TIME
        : (notInConfig(only) ?? (only.kind !== "group" ? "Only a group has a name" : null)),
    });
    entries.push({
      action: "copy_id",
      name: one ? "Copy id" : `Copy ${plural(targets.length, "id")}`,
      disabled: null,
    });
    entries.push({ separator: true });
  } else if (opts.column === "bytes") {
    entries.push({
      action: "copy_path",
      name: one ? "Copy path" : `Copy ${plural(targets.length, "path")}`,
      disabled: firstBlocked(targets, (t) => (t.revealPath ? null : "Nothing on disk yet")),
    });
    entries.push({ separator: true });
  }

  entries.push({
    action: "browse",
    name: browseLabel(only),
    disabled: !one ? ONE_AT_A_TIME : only.browseBlocked,
  });
  const claimed = targets.filter((t) => t.stopJobId).length;
  if (claimed === targets.length) {
    entries.push({
      action: "stop",
      name: one ? "Stop the sync" : `Stop ${plural(targets.length, "sync")}`,
      disabled: null,
    });
  } else {
    entries.push({
      action: "sync",
      name: "Sync now",
      disabled:
        claimed > 0
          ? `${plural(claimed, "row")} of these already syncing — stop it first, or pick the others`
          : firstBlocked(targets, (t) => t.runBlocked),
    });
  }
  entries.push({
    action: "edit",
    name: "Edit settings…",
    disabled: !one ? ONE_AT_A_TIME : only.editBlocked,
  });
  entries.push({
    action: "compare",
    name: "Compare two syncs…",
    disabled: !one ? ONE_AT_A_TIME : notComparableReason(only),
  });
  entries.push({ separator: true });
  entries.push({
    action: "log",
    name: "Show log",
    disabled: !one
      ? ONE_AT_A_TIME
      : only.kind === "applet"
        ? "An applet runs no step"
        : only.kind === "system"
          ? "The log is what Browse opens here"
          : only.kind === "group" && !only.statusFrom
            ? "No step under this group has run yet"
            : null,
  });
  entries.push({
    action: "history",
    name: "Show commit history",
    disabled: firstBlocked(targets, noStoreReason),
  });
  if (opts.canReveal) {
    entries.push({
      action: "reveal",
      name: opts.revealLabel,
      disabled: firstBlocked(targets, (t) => t.revealBlocked),
    });
  }
  entries.push({ separator: true });
  entries.push({
    action: "remove",
    name: one
      ? only.kind === "group"
        ? "Remove from config, with everything under it"
        : "Remove from config"
      : `Remove ${targets.length} entries from config`,
    disabled: firstBlocked(targets, notInConfig),
  });
  return entries;
}
