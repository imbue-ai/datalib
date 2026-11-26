// A program's described sources become catalog entries whose steps run
// that program, spelled as the config's `[[programs]]` entry spells it.

import { describe, expect, it } from "vitest";

import type { ProgramDescription } from "@/api";
import { entriesOfDescription } from "./programs";
import { buildSource, listSteps } from "./sourceSteps";

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
