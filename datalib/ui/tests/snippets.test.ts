// The Sources tab's quick-add snippets write the same shape the wizard
// does: a group and two steps under composed ids, read back by the same
// parser. A snippet that drifted from that shape would land a source
// the Manage screen could not show as one row.
import { describe, expect, it } from "vitest";
import { SNIPPETS } from "../src/config/snippets";
import { listGroups, listSteps } from "../src/config/sourceSteps";

describe("quick-add snippets", () => {
  for (const snippet of SNIPPETS) {
    it(`${snippet.label} is one group with an ingest and a render step under it`, () => {
      const text = snippet.body("latchkey");
      const [group, ...rest] = listGroups(text);
      expect(rest).toEqual([]);
      expect(group.type).toBeTruthy();
      const steps = listSteps(text);
      expect(steps.map((s) => s.id)).toEqual([
        `${group.id}/ingest`,
        `${group.id}/render_markdown`,
      ]);
      expect(steps[1].inputs).toEqual([`${group.id}/ingest`]);
      // No command: a built-in step is `datalib-step`.
      expect(text).not.toContain("command");
    });
  }

  it("keeps a snippet's preamble above the group it introduces", () => {
    const claude = SNIPPETS.find((s) => s.label === "Claude")!.body("lk");
    expect(claude.indexOf("Prerequisite")).toBeLessThan(claude.indexOf("[[groups]]"));
    expect(claude).toContain("lk services register claude-ai");
  });
});
