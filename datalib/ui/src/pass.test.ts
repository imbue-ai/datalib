import { describe, expect, it } from "vitest";
import type { Upgrade } from "./api";
import { upgradeView } from "./pass";

function upgrade(over: Partial<Upgrade>): Upgrade {
  return { migrating: false, settled: true, steps: [], rerender: [], ...over };
}

describe("upgradeView", () => {
  /** A step the launch found in a shape this version does not read is
   *  shown nothing of until it runs again; the pill keeps offering the
   *  run after the launch dialog is dismissed. */
  it("offers to re-run the steps the launch found in another shape", () => {
    const steps = ["slack/render_markdown", "unified_grid_and_search_index/unified_grid_and_search_index"];
    const view = upgradeView(upgrade({ rerender: steps }), false);
    expect(view?.action).toEqual({ label: "Re-run now", steps });
    expect(view?.rows.map((r) => r.group)).toEqual(["slack", "unified_grid_and_search_index"]);
    expect(view?.blurb).toContain("not shown");
  });

  it("says nothing once every step is in this version's shape", () => {
    expect(upgradeView(upgrade({}), false)).toBeNull();
  });

  /** A source that could not be updated is the louder news. */
  it("reports a failed update before a re-run", () => {
    const view = upgradeView(
      upgrade({
        steps: [{ step: "mail/ingest", state: "failed", error: "refused" }],
        rerender: ["slack/render_markdown"],
      }),
      false,
    );
    expect(view?.tone).toBe("failed");
    expect(view?.action).toBeUndefined();
  });
});
