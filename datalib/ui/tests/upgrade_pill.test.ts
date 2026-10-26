import { describe, expect, it } from "vitest";
import type { Upgrade } from "@/api";
import { keepsItsName, rerenderSources, sourceRows, upgradePill } from "@/upgrade";

const step = (
  step: string,
  state: Upgrade["steps"][number]["state"],
  error: string | null = null,
) => ({
  step,
  state,
  error,
});

describe("upgradePill", () => {
  /// A source counts once however many steps it has, and is finished
  /// only when every one of them is.
  it("counts sources, not steps, while the pass runs", () => {
    const pill = upgradePill(
      {
        migrating: true,
        settled: false,
        steps: [
          step("enterprise-mail/ingest", "done"),
          step("enterprise-mail/render_markdown", "done"),
          step("holodeck-slack/ingest", "done"),
          step("holodeck-slack/render_markdown", "running"),
          step("ten-forward/ingest", "waiting"),
        ],
        rerender: [],
      },
      false,
    );
    expect(pill).toEqual({ tone: "updating", label: "Updating data · 1 of 3" });
  });

  it("turns red after the pass for a source it could not update, until dismissed", () => {
    const upgrade: Upgrade = {
      migrating: false,
      settled: true,
      steps: [step("enterprise-mail/ingest", "done"), step("holodeck-slack/ingest", "failed", "x")],
      rerender: [],
    };
    expect(upgradePill(upgrade, false)).toEqual({
      tone: "failed",
      label: "1 source could not be updated",
    });
    expect(upgradePill(upgrade, true)).toBeNull();
  });

  it("is gone once a pass is over with nothing failed, or when there was none", () => {
    expect(
      upgradePill(
        { migrating: false, settled: true, steps: [step("a/ingest", "done")], rerender: [] },
        false,
      ),
    ).toBeNull();
    expect(
      upgradePill({ migrating: false, settled: true, steps: [], rerender: [] }, false),
    ).toBeNull();
  });
});

describe("the upgrade's sources", () => {
  /// The list named each source by its raw id; each row now carries the
  /// group, so it is drawn as the source's chip under its configured name.
  it("carry their group, and the search index its plain name", () => {
    const upgrade: Upgrade = {
      migrating: false,
      settled: true,
      steps: [
        step("enterprise-mail/ingest", "done"),
        step("enterprise-mail/render_markdown", "failed", "no datalib-step"),
        step("unified_document_index/grid_index", "done"),
      ],
      rerender: [
        "ten-forward/render_markdown",
        "ten-forward/keyword_index",
        "unified_document_index/grid_index",
      ],
    };
    expect(sourceRows(upgrade)).toEqual([
      {
        group: "enterprise-mail",
        source: "enterprise-mail",
        state: "failed",
        error: "no datalib-step",
      },
      { group: "unified_document_index", source: "Search index", state: "done", error: null },
    ]);
    expect(rerenderSources(upgrade)).toEqual([
      { group: "ten-forward", source: "ten-forward" },
      { group: "unified_document_index", source: "Search index" },
    ]);
    expect(keepsItsName("unified_document_index")).toBe(true);
    expect(keepsItsName("enterprise-mail")).toBe(false);
  });
});
