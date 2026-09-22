import { describe, expect, it } from "vitest";
import {
  DEFAULT_WIDTH,
  pageTitle,
  pathFor,
  reconcile,
  specOf,
  specsOf,
  type Slot,
} from "@/views/millerStack";
import type { ColumnSpec } from "@/router/columns";

let n = 0;
const slot = (source: string, state = "", width: number | null = null): Slot => ({
  id: `s${++n}`,
  source,
  state,
  width,
  title: null,
});
const fresh = (spec: ColumnSpec): Slot => slot(spec.code, spec.state);

describe("reconcile", () => {
  it("keeps a column the URL still describes, by identity", () => {
    const grid = slot("gridView()", "sel=a");
    const doc = slot('documentView("a")');
    const next = reconcile([grid, doc], specsOf([grid, doc]), fresh);
    expect(next.map((s) => s.id)).toEqual([grid.id, doc.id]);
  });

  it("drops the columns Back removed and keeps the rest mounted", () => {
    const grid = slot("gridView()", "sel=a");
    const doc = slot('documentView("a")');
    const next = reconcile([grid, doc], specsOf([grid]), fresh);
    expect(next.map((s) => s.id)).toEqual([grid.id]);
  });

  it("mounts a fresh column for one Forward brought back", () => {
    const grid = slot("gridView()", "sel=a");
    const next = reconcile([grid], [...specsOf([grid]), specOf('documentView("a")')], fresh);
    expect(next[0].id).toBe(grid.id);
    expect(next[1].source).toBe('documentView("a")');
    expect(next[1].id).not.toBe(grid.id);
  });

  it("remounts a column whose state the URL changed", () => {
    // A card reads its state once, at mount.
    const grid = slot("gridView()", "sel=a");
    const next = reconcile([grid], [{ code: "gridView()", size: null, state: "sel=b" }], fresh);
    expect(next[0].id).not.toBe(grid.id);
    expect(next[0].state).toBe("sel=b");
  });

  it("remounts a column whose code changed at its position", () => {
    const gallery = slot("galleryView()");
    const next = reconcile([gallery], [specOf("documentPickerView()")], fresh);
    expect(next[0].id).not.toBe(gallery.id);
  });

  it("takes the URL's width for a kept column", () => {
    const grid = slot("gridView()", "", 900);
    const next = reconcile([grid], [{ code: "gridView()", size: 0.5, state: "" }], fresh);
    expect(next[0].id).toBe(grid.id);
    expect(next[0].width).toBe(0.5 * DEFAULT_WIDTH);
  });
});

describe("pathFor", () => {
  it("writes the pristine default stack as /", () => {
    expect(pathFor([specOf("gridView()")])).toBe("/");
    expect(pathFor([{ code: "gridView()", size: null, state: "q=x" }])).not.toBe("/");
  });
});

describe("pageTitle", () => {
  it("puts the newest column first and the app last", () => {
    expect(pageTitle(["Search: kraken", "Warp core"])).toBe("Warp core · Search: kraken · Datalib");
    expect(pageTitle([])).toBe("Datalib");
  });
});
