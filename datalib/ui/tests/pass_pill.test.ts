import { describe, expect, it } from "vitest";
import type { FreeSpace, PassRow, PassState, Upgrade } from "@/api";
import { freeSpaceView, keepsItsName, rerenderSources, sourceRows, upgradeView } from "@/pass";

const step = (
  step: string,
  state: PassState,
  error: string | null = null,
  store_bytes?: { before: number; after: number },
): PassRow => ({ step, state, error, ...(store_bytes ? { store_bytes } : {}) });

describe("upgradeView", () => {
  /// A source counts once however many steps it has, and is finished
  /// only when every one of them is.
  it("counts sources, not steps, while the pass runs", () => {
    const view = upgradeView(
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
    expect(view).toMatchObject({ tone: "updating", label: "Updating data · 1 of 3" });
    expect(view?.rows.map((r) => r.said)).toEqual(["Done", "Updating…", "Waiting"]);
  });

  it("turns red after the pass for a source it could not update, until dismissed", () => {
    const upgrade: Upgrade = {
      migrating: false,
      settled: true,
      steps: [step("enterprise-mail/ingest", "done"), step("holodeck-slack/ingest", "failed", "x")],
      rerender: [],
    };
    const view = upgradeView(upgrade, false);
    expect(view).toMatchObject({ tone: "failed", label: "1 source could not be updated" });
    expect(view?.rows.map((r) => r.group)).toEqual(["holodeck-slack"]);
    expect(upgradeView(upgrade, true)).toBeNull();
  });

  it("is gone once a pass is over with nothing failed, or when there was none", () => {
    expect(
      upgradeView(
        { migrating: false, settled: true, steps: [step("a/ingest", "done")], rerender: [] },
        false,
      ),
    ).toBeNull();
    expect(
      upgradeView({ migrating: false, settled: true, steps: [], rerender: [] }, false),
    ).toBeNull();
  });
});

describe("freeSpaceView", () => {
  const pass = (over: Partial<FreeSpace>): FreeSpace => ({
    id: "w1",
    kind: "collect",
    running: true,
    error: null,
    steps: [],
    ...over,
  });

  /// Guards the free-space pass that said nothing for minutes: while it
  /// runs the pill counts sources done and adds up what they gave back.
  it("counts sources and the bytes freed so far while it runs", () => {
    const view = freeSpaceView(
      pass({
        steps: [
          step("enterprise-mail/ingest", "done", null, {
            before: 6_600_000_000,
            after: 3_100_000_000,
          }),
          step("enterprise-mail/render_markdown", "done", null, {
            before: 1_200_000_000,
            after: 1_000_000_000,
          }),
          step("holodeck-slack/ingest", "running"),
          step("ten-forward/ingest", "waiting"),
        ],
      }),
      false,
    );
    expect(view).toMatchObject({
      tone: "updating",
      label: "Freeing space · 1 of 3 · 3.7 GB freed",
    });
    expect(view?.rows.map((r) => r.said)).toEqual(["Done · 3.7 GB", "Freeing…", "Waiting"]);
  });

  it("says it is freeing space before the loop has taken it up", () => {
    expect(freeSpaceView(pass({}), false)).toMatchObject({
      tone: "updating",
      label: "Freeing space",
    });
  });

  /// A step with no store to shrink reports no size: done, and nothing freed.
  it("says what it gave back once over, until dismissed", () => {
    const over = pass({
      running: false,
      steps: [
        step("a/ingest", "done", null, { before: 3000, after: 1000 }),
        step("a/keyword_index", "done"),
      ],
    });
    expect(freeSpaceView(over, false)).toMatchObject({ tone: "done", label: "Freed 2.0 kB" });
    expect(freeSpaceView(over, true)).toBeNull();
    expect(freeSpaceView(null, false)).toBeNull();
  });

  it("turns red for a source it could not shrink, or a pass refused whole", () => {
    expect(
      freeSpaceView(
        pass({
          running: false,
          error: "could not free up disk in a/ingest",
          steps: [step("a/ingest", "failed", "needs about its size again")],
        }),
        false,
      ),
    ).toMatchObject({ tone: "failed", label: "1 source could not free space" });
    expect(
      freeSpaceView(pass({ running: false, error: 'no step "borg/ingest"' }), false),
    ).toMatchObject({ tone: "failed", label: "Could not free up disk space" });
  });
});

describe("the sources of a pass", () => {
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
    expect(sourceRows(upgrade.steps)).toEqual([
      {
        group: "enterprise-mail",
        source: "enterprise-mail",
        state: "failed",
        error: "no datalib-step",
        freed: null,
      },
      {
        group: "unified_document_index",
        source: "Search index",
        state: "done",
        error: null,
        freed: null,
      },
    ]);
    expect(rerenderSources(upgrade)).toEqual([
      { group: "ten-forward", source: "ten-forward" },
      { group: "unified_document_index", source: "Search index" },
    ]);
    expect(keepsItsName("unified_document_index")).toBe(true);
    expect(keepsItsName("enterprise-mail")).toBe(false);
  });
});
