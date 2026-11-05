import { describe, expect, it } from "vitest";

import { listGroups } from "@/config/sourceSteps";
import { PRESET_LIMIT, presetsFor } from "./dactalPresets";

const CONFIG = `
[[groups]]
id = "work"
name = "Work Slack"
type = "slack"

[[groups]]
id = "mail"
type = "email"

[[groups]]
id = "unified_document_index"
`;

describe("the DACTAL card's canned queries", () => {
  it("offers none when the config has no Slack source", () => {
    expect(presetsFor(listGroups('[[groups]]\nid = "mail"\ntype = "email"\n'))).toEqual([]);
  });

  it("loads the Slack source by its id", () => {
    const presets = presetsFor(listGroups(CONFIG));
    expect(presets.length).toBeGreaterThan(0);
    for (const p of presets) {
      expect(p).toMatchObject({ group: "Slack", load: "source:work", limit: PRESET_LIMIT });
    }
  });

  it("gives each Slack source its own set, named", () => {
    const two = `${CONFIG}\n[[groups]]\nid = "home"\ntype = "slack"\n`;
    const groups = new Set(presetsFor(listGroups(two)).map((p) => `${p.group} ${p.load}`));
    expect(groups).toEqual(new Set(["Slack: Work Slack source:work", "Slack: home source:home"]));
  });
});
