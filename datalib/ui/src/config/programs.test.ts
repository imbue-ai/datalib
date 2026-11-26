// A program's described sources become catalog entries whose steps run
// that program, spelled as the config's `[[programs]]` entry spells it.

import { describe, expect, it } from "vitest";

import type { ProgramDescription } from "@/api";
import { filterCatalog } from "./catalog";
import { entriesOfDescription, useDescriptions } from "./programs";
import { buildSource, insertEntries, listGroups, listSteps, setSourceSteps } from "./sourceSteps";

const TIDES: ProgramDescription = {
  sources: [
    {
      type: "tide",
      commands: {
        ingest: { command: "run ingest tide" },
        render_markdown: { command: "run render_markdown tide" },
      },
      probe: "probe tide",
      wizard: { label: "Tide gauge", kind: "api", wizard: true },
      applets: [],
    },
    {
      type: "pool",
      commands: { ingest: { command: "run ingest pool" } },
      wizard: { label: "Tide pool" },
      applets: [],
    },
  ],
};

describe("a program's sources", () => {
  const [tide, pool] = entriesOfDescription("/opt/bin/tides", TIDES);

  it("are entries drawn by their own form, probed where they name a probe", () => {
    expect(tide).toMatchObject({ type: "tide", label: "Tide gauge", kind: "api", canProbe: true });
    expect(pool).toMatchObject({ type: "pool", canProbe: false, wizard: false });
  });

  it("write steps that run the program with the words its description gives", () => {
    const src = buildSource({ entry: tide, group: "sf", name: "SF", values: {}, withGroup: true });
    const steps = listSteps(`${src.groupBody}\n\n${src.stepsBody}`);
    expect(steps.map((s) => [s.id, s.command])).toEqual([
      ["sf/ingest", "/opt/bin/tides run ingest tide"],
      ["sf/render_markdown", "/opt/bin/tides run render_markdown tide"],
    ]);
  });

  it("get no render step when the program has no command for one", () => {
    const src = buildSource({ entry: pool, group: "p", name: "P", values: {}, withGroup: true });
    expect(src.renderId).toBeNull();
  });
});

/// The rules for which steps a source gets, how each reads and what it
/// locks, and the fan-ins they feed, used to be compiled into the wizard
/// for datalib-step alone. Now they come from the description: a made-up
/// program gets exactly what it describes.
describe("the steps a description gives a source", () => {
  const GAUGES: ProgramDescription = {
    sources: [
      {
        type: "gauge",
        commands: {
          ingest: { command: "pull gauge", locks: ["noaa"] },
          render_markdown: { command: "draw gauge", reads: "files", inputs: ["ingest"] },
          render_time_series: { command: "chart gauge", reads: "pinned" },
        },
        wizard: { label: "Gauge", kind: "api", wizard: true },
        applets: [],
      },
    ],
    locks: { noaa: 3, series: 1 },
    fan_ins: [
      {
        group: "all_series",
        name: "All series",
        function: "index_series",
        command: { command: "index", inputs: ["*/render_time_series"], locks: ["series"] },
        applet: "gauges-applet",
        added_with_a_source: true,
      },
    ],
  };

  it("are its steps, with its reads, locks and fan-ins", () => {
    useDescriptions({ programs: [{ command: "gauges", description: GAUGES, error: null }] });
    try {
      const entry = filterCatalog("").find((e) => e.type === "gauge")!;
      const src = buildSource({ entry, group: "g", name: "G", values: {}, withGroup: true });
      let text = insertEntries("", `${src.groupBody}\n\n${src.stepsBody}`);
      text = setSourceSteps(text, "g", entry);
      const step = (id: string) => text.split("[[steps]]").find((s) => s.includes(`"${id}"`))!;

      expect(step("g/ingest")).toContain('command = "gauges pull gauge"');
      expect(step("g/ingest")).toContain('locks = ["noaa"]');
      expect(step("g/render_markdown")).toContain('reads = "files"');
      expect(step("g/render_time_series")).toContain('inputs = ["g/ingest"]');
      expect(step("all_series/index_series")).toContain('command = "gauges index"');
      expect(step("all_series/index_series")).toContain('locks = ["series"]');
      expect(text).toContain('[[locks]]\nname = "noaa"\nslots = 3');
      expect(text).toContain('[[locks]]\nname = "series"\nslots = 1');
      expect(listGroups(text).map((g) => g.id)).toContain("all_series");
      expect(listSteps(text).find((s) => s.kind === "applet")?.command).toBe("gauges-applet");
      expect(setSourceSteps(text, "g", entry)).toBe(text);
    } finally {
      useDescriptions({ programs: [] });
    }
  });
});
