import { describe, expect, it } from "vitest";
import { noStoreReason, rowMenu, type MenuEntry, type MenuTarget } from "./rowMenu";

const target = (over: Partial<MenuTarget> = {}): MenuTarget => ({
  id: "slack",
  name: "Work Slack",
  kind: "group",
  type: "slack",
  func: null,
  runBlocked: null,
  editBlocked: null,
  revealBlocked: null,
  browseBlocked: null,
  rawStore: false,
  stopRequestIds: [],
  turnedOffBy: null,
  statusFrom: "slack/render_markdown",
  revealPath: "/data/slack",
  ...over,
});

const opts = { canReveal: true, revealLabel: "Reveal in Finder" };

const has = (menu: MenuEntry[], action: string) =>
  menu.some((m) => !m.separator && m.action === action);

function entry(menu: MenuEntry[], action: string) {
  const e = menu.find((m) => !m.separator && m.action === action);
  if (!e || e.separator) throw new Error(`no ${action} in ${JSON.stringify(menu)}`);
  return e;
}

describe("rowMenu", () => {
  it("offers every row action, enabled, for one ordinary group, grouped by what it touches", () => {
    const menu = rowMenu([target()], opts);
    const actions = menu.map((m) => (m.separator ? "—" : m.action));
    expect(actions).toEqual([
      "browse",
      "sync",
      "turn_off",
      "—",
      "edit",
      "rename",
      "—",
      "history",
      "compare",
      "log",
      "—",
      "reveal",
      "copy_path",
      "copy_id",
      "—",
      "reset",
      "remove",
    ]);
    for (const m of menu) if (!m.separator) expect(m.disabled).toBeNull();
  });

  it("leaves out an entry for another kind of row, and greys one that cannot run now", () => {
    const applet = rowMenu([target({ kind: "applet", func: null, statusFrom: null })], opts);
    expect(has(applet, "history")).toBe(false);
    expect(has(applet, "log")).toBe(false);
    expect(has(rowMenu([target({ kind: "step", func: "qmd_aggregator" })], opts), "history")).toBe(
      false,
    );
    expect(
      entry(rowMenu([target({ runBlocked: "Not in the pipeline" })], opts), "sync").disabled,
    ).toBe("Not in the pipeline");
    expect(entry(rowMenu([target({ statusFrom: null })], opts), "log").disabled).toBe(
      "No step under this group has run yet",
    );
  });

  /// Left-out entries once left their separators behind: a menu that
  /// opened on a line, or showed two lines with nothing between them.
  it("draws no separator first, last, or twice in a row", () => {
    const shapes = [
      [target({ kind: "applet", func: null, statusFrom: null })],
      [target({ id: "system", name: "System", kind: "system", type: null, statusFrom: null })],
      [target(), target({ id: "b" })],
    ];
    for (const targets of shapes) {
      const lines = rowMenu(targets, { ...opts, canReveal: false }).map((m) => !!m.separator);
      expect(lines[0]).toBe(false);
      expect(lines[lines.length - 1]).toBe(false);
      lines.slice(1).forEach((line, i) => expect(line && lines[i]).toBe(false));
    }
  });

  it("offers Reset on a source and its steps, and nowhere else", () => {
    const index = target({ id: "unified_index", type: null, statusFrom: null });
    expect(has(rowMenu([index], opts), "reset")).toBe(false);
    const busy = target({ stopRequestIds: ["req-1"] });
    expect(entry(rowMenu([busy], opts), "reset").disabled).toBe("Busy — stop the sync first");
    const ingest = target({ kind: "step", func: "ingest" });
    expect(entry(rowMenu([ingest], opts), "reset").disabled).toBeNull();
    const render = target({ kind: "step", func: "render_markdown" });
    expect(entry(rowMenu([render], opts), "reset").disabled).toBeNull();
    const diff = target({ type: "diff" });
    expect(entry(rowMenu([diff], opts), "reset").disabled).toBeNull();
  });

  it("offers Reset on the embedding map, the one index step a person resets", () => {
    const map = target({
      id: "unified_index/embedding_map",
      kind: "step",
      type: null,
      func: "embedding_map",
    });
    expect(entry(rowMenu([map], opts), "reset").disabled).toBeNull();
    const qmd = target({
      id: "unified_index/qmd_aggregator",
      kind: "step",
      type: null,
      func: "qmd_aggregator",
    });
    expect(has(rowMenu([qmd], opts), "reset")).toBe(false);
  });

  it("offers the system row its log and its path, and nothing that edits the config", () => {
    const menu = rowMenu(
      [target({ id: "system", name: "System", kind: "system", type: null, statusFrom: null })],
      opts,
    );
    expect(menu.map((m) => (m.separator ? "—" : m.action))).toEqual([
      "browse",
      "—",
      "reveal",
      "copy_path",
      "copy_id",
    ]);
    expect(entry(menu, "browse").name).toBe("Browse the log");
    // Several rows with the system row among them: the reason names it.
    const mixed = rowMenu(
      [target(), target({ id: "system", name: "System", kind: "system" })],
      opts,
    );
    expect(entry(mixed, "remove").disabled).toBe("System: Not a config entry");
  });

  it("names Browse for what it opens on a download step with a raw store", () => {
    const step = { kind: "step" as const, func: "ingest", id: "slack/ingest" };
    expect(entry(rowMenu([target(step)], opts), "browse").name).toBe("Browse this data");
    expect(entry(rowMenu([target({ ...step, rawStore: true })], opts), "browse").name).toBe(
      "Browse the downloaded tables",
    );
  });

  it("leaves out the one-row actions when several rows are targeted, and names the row a reason came from", () => {
    const menu = rowMenu([target(), target({ id: "mail", name: "Mail", editBlocked: "x" })], opts);
    for (const action of ["browse", "edit", "rename", "compare", "log"]) {
      expect(has(menu, action)).toBe(false);
    }
    expect(entry(menu, "history").disabled).toBeNull();
    expect(entry(menu, "remove").name).toBe("Remove 2 entries from config");
    const blocked = rowMenu([target(), target({ id: "mail", name: "Mail", kind: "applet" })], opts);
    expect(entry(blocked, "history").disabled).toBe("Mail: An applet writes no store");
  });

  it("offers Compare on a source and nothing else", () => {
    expect(entry(rowMenu([target()], opts), "compare").disabled).toBeNull();
    expect(has(rowMenu([target({ kind: "step", func: "ingest" })], opts), "compare")).toBe(false);
    expect(has(rowMenu([target({ type: null })], opts), "compare")).toBe(false);
    expect(has(rowMenu([target({ type: "diff" })], opts), "compare")).toBe(false);
  });

  it("turns Sync into Stop only when every target is claimed", () => {
    expect(entry(rowMenu([target({ stopRequestIds: ["r1"] })], opts), "stop").name).toBe(
      "Stop the sync",
    );
    const mixed = rowMenu(
      [target({ stopRequestIds: ["r1"] }), target({ id: "mail", name: "Mail" })],
      opts,
    );
    expect(entry(mixed, "sync").disabled).toMatch(/already syncing/);
  });

  it("offers Turn on only when every target is off, and Turn off on nothing unscheduled", () => {
    expect(
      entry(rowMenu([target({ turnedOffBy: "claude" })], opts), "turn_on").disabled,
    ).toBeNull();
    const mixed = rowMenu([target({ turnedOffBy: "ui" }), target({ id: "mail" })], opts);
    expect(entry(mixed, "turn_off").disabled).toBeNull();
    expect(has(rowMenu([target({ kind: "applet" })], opts), "turn_off")).toBe(false);
  });

  it("offers Rename on a group, and the copies wherever the pointer is", () => {
    expect(has(rowMenu([target({ kind: "step" })], opts), "rename")).toBe(false);
    const two = rowMenu([target(), target({ id: "b", name: "B", revealPath: null })], opts);
    expect(entry(two, "copy_path")).toMatchObject({
      name: "Copy 2 paths",
      disabled: "B: Nothing on disk yet",
    });
    expect(entry(two, "copy_id")).toMatchObject({ name: "Copy 2 ids", disabled: null });
  });

  it("omits Reveal where the host cannot reveal", () => {
    expect(has(rowMenu([target()], { ...opts, canReveal: false }), "reveal")).toBe(false);
  });

  it("names the trees that keep no store", () => {
    expect(noStoreReason(target({ kind: "step", func: "ingest" }))).toBeNull();
    expect(noStoreReason(target({ kind: "step", func: "grid_index" }))).toBeNull();
  });
});
