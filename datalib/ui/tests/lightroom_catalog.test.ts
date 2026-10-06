// One Lightroom form writes a catalog, a folder of backups, or both:
// the backend replays the backups and mirrors the catalog on top.
import { describe, expect, it } from "vitest";
import { CATALOG, catalogForStep } from "../src/config/catalog";
import { buildStep, seedFieldValues } from "../src/config/sourceSteps";
import methods from "../src/config/ingestMethods.json";

const LIGHTROOM = CATALOG.filter((e) => e.type === "lightroom");
const entry = LIGHTROOM[0];

const write = (values: Record<string, unknown>) =>
  buildStep({
    entry,
    group: "lr",
    phase: "download",
    values: { ...seedFieldValues(entry), ...values },
  });

describe("the lightroom form", () => {
  it("is one entry with a path field for every method the backend declares", () => {
    expect(LIGHTROOM).toHaveLength(1);
    const declared = (methods as Record<string, { path: string }[]>).lightroom.map(
      (m) => `${m.path}.path`,
    );
    const paths = entry.fields!.filter((f) => f.kind === "path");
    expect(paths.map((f) => f.target).sort()).toEqual([...declared].sort());
    expect(paths.every((f) => !("required" in f && f.required))).toBe(true);
    expect([...entry.requiresOneOf!].sort()).toEqual([...declared].sort());
  });

  it("picks a catalog file, zip included, or a backups folder", () => {
    const field = (t: string) => entry.fields!.find((f) => f.target === t)!;
    const catalog = field("catalog.path");
    expect(catalog.kind === "path" && catalog.picks).toBe("file");
    expect(catalog.kind === "path" && catalog.extensions).toEqual(["lrcat", "zip"]);
    const backups = field("backups.path");
    expect(backups.kind === "path" && backups.picks).toBe("dir");
  });

  it("writes only the tables that were filled in", () => {
    const both = write({
      "catalog.path": "~/Pictures/Lightroom/Enterprise.lrcat",
      "backups.path": "~/Pictures/Lightroom/Backups",
    });
    expect(both).toContain("[steps.params.catalog]");
    expect(both).toContain("[steps.params.backups]");
    const backupsOnly = write({ "backups.path": "~/Pictures/Lightroom/Backups" });
    expect(backupsOnly).toContain("[steps.params.backups]");
    expect(backupsOnly).not.toContain("[steps.params.catalog]");
  });

  it("is the form for a step holding either table or both", () => {
    for (const params of [
      { catalog: { path: "x.lrcat" } },
      { backups: { path: "B" } },
      { catalog: { path: "x.lrcat" }, backups: { path: "B" } },
    ]) {
      expect(catalogForStep("lightroom", params)).toBe(entry);
    }
  });
});
