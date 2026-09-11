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
  stopJobId: null,
  statusFrom: "slack/render_markdown",
  revealPath: "/data/slack",
  ...over,
});

const opts = { column: "status", canReveal: true, revealLabel: "Reveal in Finder" };

function entry(menu: MenuEntry[], action: string) {
  const e = menu.find((m) => !m.separator && m.action === action);
  if (!e || e.separator) throw new Error(`no ${action} in ${JSON.stringify(menu)}`);
  return e;
}

describe("rowMenu", () => {
  it("offers every row action, enabled, for one ordinary group", () => {
    const menu = rowMenu([target()], opts);
    const actions = menu.filter((m) => !m.separator).map((m) => !m.separator && m.action);
    expect(actions).toEqual(["browse", "sync", "edit", "log", "history", "reveal", "remove"]);
    for (const m of menu) if (!m.separator) expect(m.disabled).toBeNull();
  });

  it("keeps an entry that does not apply, disabled with the reason", () => {
    const menu = rowMenu([target({ kind: "applet", func: null, statusFrom: null })], opts);
    expect(entry(menu, "history").disabled).toBe("An applet writes no store");
    expect(entry(menu, "log").disabled).toBe("An applet runs no step");
    expect(entry(rowMenu([target({ kind: "step", func: "qmd_index" })], opts), "history").disabled).toBe(
      "The QMD index keeps no doltlite store",
    );
    expect(entry(rowMenu([target({ runBlocked: "Not in the pipeline" })], opts), "sync").disabled).toBe(
      "Not in the pipeline",
    );
  });

  it("limits the one-row actions when several rows are targeted, and names the row a reason came from", () => {
    const menu = rowMenu([target(), target({ id: "mail", name: "Mail", editBlocked: "x" })], opts);
    expect(entry(menu, "browse").disabled).toBe("One row at a time");
    expect(entry(menu, "edit").disabled).toBe("One row at a time");
    expect(entry(menu, "log").disabled).toBe("One row at a time");
    expect(entry(menu, "history").disabled).toBeNull();
    expect(entry(menu, "remove").name).toBe("Remove 2 entries from config");
    const blocked = rowMenu([target(), target({ id: "mail", name: "Mail", kind: "applet" })], opts);
    expect(entry(blocked, "history").disabled).toBe("Mail: An applet writes no store");
  });

  it("turns Sync into Stop only when every target is claimed", () => {
    expect(entry(rowMenu([target({ stopJobId: "j1" })], opts), "stop").name).toBe("Stop the sync");
    const mixed = rowMenu([target({ stopJobId: "j1" }), target({ id: "mail", name: "Mail" })], opts);
    expect(entry(mixed, "sync").disabled).toMatch(/already syncing/);
  });

  it("adds the cell's own entries ahead of the row's", () => {
    const name = rowMenu([target()], { ...opts, column: "name" });
    expect(name[0]).toMatchObject({ action: "rename", disabled: null });
    expect(name[1]).toMatchObject({ action: "copy_id", name: "Copy id" });
    expect(name[2]).toEqual({ separator: true });
    expect(entry(rowMenu([target({ kind: "step" })], { ...opts, column: "name" }), "rename").disabled).toBe(
      "Only a group has a name",
    );
    const bytes = rowMenu([target(), target({ id: "b", name: "B", revealPath: null })], {
      ...opts,
      column: "bytes",
    });
    expect(bytes[0]).toMatchObject({ action: "copy_path", name: "Copy 2 paths", disabled: "B: Nothing on disk yet" });
    expect(rowMenu([target()], opts).some((m) => !m.separator && m.action === "rename")).toBe(false);
  });

  it("omits Reveal where the host cannot reveal", () => {
    const menu = rowMenu([target()], { ...opts, canReveal: false });
    expect(menu.some((m) => !m.separator && m.action === "reveal")).toBe(false);
  });

  it("names the trees that keep no store", () => {
    expect(noStoreReason(target({ kind: "step", func: "ingest" }))).toBeNull();
    expect(noStoreReason(target({ kind: "step", func: "grid_index" }))).toBeNull();
  });
});
